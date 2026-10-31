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
    channel: Vec<ContextItem>,
    teammate: Vec<ContextItem>,
    agent: Vec<ContextItem>,
}

/// One piece of context waiting for the next turn.
#[derive(Debug, Clone)]
pub struct ContextItem {
    /// What the model reads.
    pub text: String,
    /// Whether this item is a reason to start a turn.
    ///
    /// An agent message that arrived while nobody could start a turn is not:
    /// the hop or wake bounds refused it a turn of its own, so it waits for one
    /// somebody else starts and rides into that. It is still carried — a
    /// message that was accepted is never dropped — it just cannot be the
    /// reason the turn exists.
    pub may_wake: bool,
    /// The session whose message this is, and how deep it already came, when
    /// it is another agent's. The turn it rides into answers that session, so
    /// its reply carries one more hop.
    pub inbound: Option<(String, u8)>,
    /// The message's own id, when it is another agent's: a turn it starts
    /// names its prompt row after it.
    pub agent_message_id: Option<String>,
}

impl ContextItem {
    /// Context that is a reason to start a turn and answers nobody: a channel
    /// notification or a teammate's mailbox message.
    pub fn waking(text: String) -> Self {
        Self {
            text,
            may_wake: true,
            inbound: None,
            agent_message_id: None,
        }
    }

    /// A message from another agent session, which the turn carrying it
    /// answers.
    pub fn agent(message_id: String, text: String, from_session_id: String, hops: u8) -> Self {
        Self {
            text,
            may_wake: true,
            inbound: Some((from_session_id, hops)),
            agent_message_id: Some(message_id),
        }
    }

    /// The same message, but not a reason to start a turn: the bounds refused
    /// it one, so it waits for a turn somebody else starts.
    pub fn deferred(mut self) -> Self {
        self.may_wake = false;
        self
    }
}

impl PendingContext {
    fn queue(&mut self, source: ContextSource) -> &mut Vec<ContextItem> {
        match source {
            ContextSource::Channel => &mut self.channel,
            ContextSource::Teammate => &mut self.teammate,
            ContextSource::Agent => &mut self.agent,
        }
    }

    fn may_wake(&self) -> bool {
        self.channel
            .iter()
            .chain(&self.teammate)
            .chain(&self.agent)
            .any(|item| item.may_wake)
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
        self.push_items(source, prompts.into_iter().map(ContextItem::waking));
    }

    /// Add context items that decide for themselves whether they may start a
    /// turn.
    pub fn push_items(&self, source: ContextSource, items: impl IntoIterator<Item = ContextItem>) {
        self.lock().queue(source).extend(items);
    }

    /// Add messages other agent sessions sent, settling for each whether it may
    /// be the reason for a turn.
    ///
    /// One the bounds admitted may start a turn and one they refused may not.
    /// One nobody judged yet — it joined a running turn that ended before
    /// reading it, or it waited behind a turn already owed — is judged here, but
    /// only if it would be the reason: when something waiting already starts a
    /// turn, it rides into that one and spends nothing. Arriving while a turn
    /// ran is therefore never a way past the bounds. Judged in arrival order,
    /// and the decision and the check of what is already owed happen under the
    /// inbox's lock, so a batch spends at most the one wake it needs.
    pub fn push_agent_messages(
        &self,
        messages: impl IntoIterator<Item = crate::mid_turn_queue::DeferredAgentMessage>,
        ledger: &rebon_tool::peer::AgentReplyLedger,
    ) {
        use rebon_tool::peer::WakeVerdict;
        let messages = messages.into_iter().collect::<Vec<_>>();
        let mut context = self.lock();
        let mut turn_owed = context.may_wake()
            || messages
                .iter()
                .any(|message| message.verdict == WakeVerdict::Admitted);
        for message in messages {
            let may_wake = match message.verdict {
                WakeVerdict::Admitted => true,
                WakeVerdict::Refused => false,
                WakeVerdict::Unjudged if turn_owed => false,
                WakeVerdict::Unjudged => {
                    let admitted = ledger.judge_wake(
                        &message.from_session_id,
                        message.hops,
                        rebon_types::wall_clock_ms(),
                    ) == WakeVerdict::Admitted;
                    turn_owed |= admitted;
                    admitted
                }
            };
            context.agent.push(ContextItem {
                text: message.text,
                may_wake,
                inbound: Some((message.from_session_id, message.hops)),
                agent_message_id: Some(message.message_id),
            });
        }
    }

    /// Whether any context is waiting, including context that may not start a
    /// turn.
    pub fn has_context(&self) -> bool {
        let context = self.lock();
        !(context.channel.is_empty() && context.teammate.is_empty() && context.agent.is_empty())
    }

    /// Whether any waiting context is a reason to start a turn.
    ///
    /// A message the bounds refused a turn of its own stays in the inbox and is
    /// carried by the next turn somebody else starts; asking this rather than
    /// [`Self::has_context`] is what keeps it from starting that turn itself.
    pub fn has_waking_context(&self) -> bool {
        self.lock().may_wake()
    }

    /// Take everything waiting, for the turn about to start: channel
    /// notifications first, then teammate messages, then agent messages.
    pub fn take_context(&self) -> Vec<String> {
        self.take_context_items()
            .into_iter()
            .map(|item| item.text)
            .collect()
    }

    /// Take everything waiting, with what each item says about the turn that
    /// carries it.
    pub fn take_context_items(&self) -> Vec<ContextItem> {
        let mut context = self.lock();
        let mut taken = std::mem::take(&mut context.channel);
        taken.append(&mut context.teammate);
        taken.append(&mut context.agent);
        taken
    }

    /// A copy of what is waiting from one source, leaving it in place.
    pub fn pending_context(&self, source: ContextSource) -> Vec<String> {
        self.lock()
            .queue(source)
            .iter()
            .map(|item| item.text.clone())
            .collect()
    }

    /// A copy of what is waiting from one source as items.
    pub fn pending_context_items(&self, source: ContextSource) -> Vec<ContextItem> {
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
            ReadyInput::Context => self.has_waking_context(),
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
    use crate::mid_turn_queue::DeferredAgentMessage;
    use rebon_tool::peer::{
        AgentReplyLedger, WakeVerdict, AGENT_MESSAGE_WAKES_PER_HOUR, MAX_AGENT_MESSAGE_HOPS,
    };

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

    /// A message the bounds refused a turn of its own is still waiting, but it
    /// is not a reason to start one: it rides into the next turn somebody else
    /// starts, which is the only way it is ever delivered.
    #[test]
    fn a_deferred_message_waits_without_starting_a_turn() {
        let inbox = SessionInbox::default();
        let cron = rebon_core::cron::CronPoller::default();
        assert!(inbox.take_task_notification_look());

        inbox.push_items(
            ContextSource::Agent,
            [
                ContextItem::agent("m-refused".into(), "refused".into(), "sess-peer".into(), 9)
                    .deferred(),
            ],
        );
        assert!(inbox.has_context(), "the message is kept");
        assert!(!inbox.has_waking_context());
        assert_eq!(inbox.ready_inputs(&cron).count(), 0);

        // Only a later waking item turns the idle path on; the deferred one
        // still comes along.
        inbox.push_context(ContextSource::Teammate, ["hello".into()]);
        assert_eq!(inbox.ready_inputs(&cron).next(), Some(ReadyInput::Context));
        let items = inbox.take_context_items();
        assert_eq!(
            items
                .iter()
                .map(|item| (item.text.as_str(), item.may_wake))
                .collect::<Vec<_>>(),
            [("hello", true), ("refused", false)]
        );
        assert_eq!(
            items[1].inbound,
            Some(("sess-peer".to_string(), 9)),
            "the depth travels with the message, not with the turn"
        );
        assert!(!inbox.has_context());
    }

    fn waiting(text: &str, hops: u8, verdict: WakeVerdict) -> DeferredAgentMessage {
        DeferredAgentMessage {
            message_id: format!("m-{text}"),
            text: text.into(),
            from_session_id: "sess-peer".into(),
            hops,
            verdict,
        }
    }

    fn agent_wakes(inbox: &SessionInbox) -> Vec<(String, bool)> {
        inbox
            .pending_context_items(ContextSource::Agent)
            .into_iter()
            .map(|item| (item.text, item.may_wake))
            .collect()
    }

    /// A message that joined a running turn and came back unread was never
    /// judged. It must be before it may start a turn of its own: a peer whose
    /// budget is spent cannot get a turn by writing while one runs.
    #[test]
    fn an_unjudged_message_is_judged_before_it_may_wake() {
        let ledger = AgentReplyLedger::new();
        for _ in 0..AGENT_MESSAGE_WAKES_PER_HOUR {
            assert!(ledger
                .admit_wake("sess-peer", 0, rebon_types::wall_clock_ms())
                .is_allowed());
        }
        let inbox = SessionInbox::default();
        inbox.push_agent_messages([waiting("returned", 0, WakeVerdict::Unjudged)], &ledger);
        assert_eq!(agent_wakes(&inbox), [("returned".to_string(), false)]);
        assert!(inbox.has_context(), "refused, not dropped");
        assert!(!inbox.has_waking_context());

        let deep = SessionInbox::default();
        deep.push_agent_messages(
            [waiting(
                "too deep",
                MAX_AGENT_MESSAGE_HOPS + 1,
                WakeVerdict::Unjudged,
            )],
            &AgentReplyLedger::new(),
        );
        assert!(
            !deep.has_waking_context(),
            "the hop bound applies to it too"
        );
    }

    /// Within the bounds, a returned message is judged and may start the turn
    /// it was waiting for, spending one wake.
    #[test]
    fn an_unjudged_message_within_the_bounds_may_wake_and_spends_one_wake() {
        let ledger = AgentReplyLedger::new();
        let inbox = SessionInbox::default();
        inbox.push_agent_messages([waiting("returned", 1, WakeVerdict::Unjudged)], &ledger);
        assert_eq!(agent_wakes(&inbox), [("returned".to_string(), true)]);
        for spent in 1..AGENT_MESSAGE_WAKES_PER_HOUR {
            assert!(
                ledger
                    .admit_wake("sess-peer", 0, rebon_types::wall_clock_ms())
                    .is_allowed(),
                "wake {spent}"
            );
        }
        assert!(
            !ledger
                .admit_wake("sess-peer", 0, rebon_types::wall_clock_ms())
                .is_allowed(),
            "the judged message spent exactly one wake"
        );
    }

    /// Nothing is judged — and nothing spent — for a message that rides into a
    /// turn already owed, whether that turn is owed to something already waiting
    /// or to an admitted message in the same batch.
    #[test]
    fn an_unjudged_message_behind_an_owed_turn_rides_along_without_spending() {
        let ledger = AgentReplyLedger::new();
        let inbox = SessionInbox::default();
        inbox.push_agent_messages(
            [
                waiting("returned", 0, WakeVerdict::Unjudged),
                waiting("admitted", 0, WakeVerdict::Admitted),
                waiting("refused", 9, WakeVerdict::Refused),
            ],
            &ledger,
        );
        assert_eq!(
            agent_wakes(&inbox),
            [
                ("returned".to_string(), false),
                ("admitted".to_string(), true),
                ("refused".to_string(), false),
            ]
        );

        let teammate_first = SessionInbox::default();
        teammate_first.push_context(ContextSource::Teammate, ["hello".into()]);
        teammate_first
            .push_agent_messages([waiting("returned", 0, WakeVerdict::Unjudged)], &ledger);
        assert_eq!(
            agent_wakes(&teammate_first),
            [("returned".to_string(), false)]
        );

        for spent in 0..AGENT_MESSAGE_WAKES_PER_HOUR {
            assert!(
                ledger
                    .admit_wake("sess-peer", 0, rebon_types::wall_clock_ms())
                    .is_allowed(),
                "wake {spent}: nothing above spent one"
            );
        }
    }

    /// A refused message that is already waiting is not a turn owed: a later
    /// unjudged one is still judged on its own.
    #[test]
    fn a_refused_message_waiting_does_not_hold_back_a_later_one() {
        let ledger = AgentReplyLedger::new();
        let inbox = SessionInbox::default();
        inbox.push_agent_messages([waiting("refused", 9, WakeVerdict::Refused)], &ledger);
        inbox.push_agent_messages([waiting("returned", 0, WakeVerdict::Unjudged)], &ledger);
        assert_eq!(
            agent_wakes(&inbox),
            [
                ("refused".to_string(), false),
                ("returned".to_string(), true),
            ]
        );
    }
}
