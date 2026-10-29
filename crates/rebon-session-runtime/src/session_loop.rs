//! The session loop: the one place this session's turns run.
//!
//! A turn is one call into the query loop — the model, its tool rounds, the
//! stop hooks and in-turn compaction all happen inside it. What happens
//! *between* turns used to have no owner: every surface (the terminal, ACP,
//! the hosted worker) decided on its own when the next turn starts and ran it
//! itself. The loop is that owner: it takes turns and history work
//! (compaction) one at a time and runs each to the end before the next, so a
//! session's history has a single writer.
//!
//! The loop itself is [`rebon_agent_core::turn_loop::TurnLoop`], shared with
//! ACP. This module is what the terminal and the hosted worker wrap around
//! it: the runtime a turn runs on, what a turn delivers and how that is
//! acknowledged, and the [`SessionInbox`] of inputs waiting for the next
//! turn.

use std::sync::Arc;

use rebon_agent_core::{PromptExecutorError, PromptRequest};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::runtime::SessionRuntime;
use crate::session_inbox::SessionInbox;

/// What produced a turn's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOrigin {
    /// The user submitted a prompt.
    User,
    /// A slash command expanded into a prompt.
    Command,
    /// A prompt the user queued while another turn ran.
    Queued,
    /// A follow-up the session queued for itself (goal, internal submits).
    FollowUp,
    /// Context that arrived while idle: teammate, channel or agent messages.
    IdleContext,
    /// Background tasks reporting back.
    TaskNotification,
    /// A cron task that fired.
    Scheduled,
    /// Replaying tool calls after a permission answer.
    PermissionReplay,
    /// A prompt sent from the agent view.
    AgentViewAttachment,
}

impl TurnOrigin {
    /// The name diagnostics give a turn of this origin.
    fn diagnostic_kind(self) -> &'static str {
        match self {
            TurnOrigin::PermissionReplay => "permission replay",
            _ => "main",
        }
    }
}

pub use rebon_agent_core::turn_loop::{HistoryWork, TurnResult};
use rebon_agent_core::turn_loop::{TurnJob, TurnLoop};

/// One turn for the loop to run.
pub struct TurnInput {
    /// The runtime the turn runs on. Carried with the input because the
    /// session can install a new runtime between turns (`/model`, resume),
    /// and a turn runs on the one it was admitted with.
    pub runtime: Arc<SessionRuntime>,
    pub request: PromptRequest,
    pub origin: TurnOrigin,
    /// What the turn delivers that has to be acknowledged once it ends.
    pub deliveries: TurnDeliveries,
}

/// Task notifications a turn carries, reserved for it in the poller.
pub struct TaskNotificationClaim {
    pub poller: Arc<crate::task_notification_poller::TaskNotificationPoller>,
    /// The session and turn the claims were reserved under.
    pub session_id: String,
    pub turn_id: String,
    pub task_ids: Vec<rebon_plugin_tasks::runtime::TaskId>,
}

/// Question escalations a turn carries.
pub struct EscalationDelivery {
    pub registry: rebon_tool::EscalationRegistry,
    pub ids: Vec<rebon_tool::EscalationId>,
}

/// What a turn delivers that is acknowledged when it ends, by the loop,
/// before anyone hears how it ended: so a surface that stops watching the
/// turn leaves nothing unsettled, and one that sees the result sees the
/// acknowledgement already made.
#[derive(Default)]
pub struct TurnDeliveries {
    pub task_notifications: Option<TaskNotificationClaim>,
    pub escalations: Option<EscalationDelivery>,
}

/// How a turn ended, as far as its deliveries care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnEnding {
    Completed,
    Cancelled,
    Failed,
}

impl TurnEnding {
    fn of(result: &TurnResult) -> Self {
        match result {
            Ok(outcome) if outcome.stop_reason == rebon_types::StopReason::Cancelled => {
                TurnEnding::Cancelled
            }
            Ok(_) => TurnEnding::Completed,
            Err(PromptExecutorError::Cancelled) => TurnEnding::Cancelled,
            Err(_) => TurnEnding::Failed,
        }
    }
}

impl TurnDeliveries {
    fn carries_anything(&self) -> bool {
        self.task_notifications
            .as_ref()
            .is_some_and(|claim| !claim.task_ids.is_empty())
            || self
                .escalations
                .as_ref()
                .is_some_and(|escalations| !escalations.ids.is_empty())
    }

    /// Acknowledge what the turn delivered.
    ///
    /// A completed turn delivered everything. A failed one delivered
    /// nothing, and what it carried is offered again after the inbox's
    /// retry. A cancelled one hands its task notifications back at once;
    /// its question escalations count as delivered, since the person who
    /// stopped the turn saw them.
    fn settle(self, ending: TurnEnding, inbox: &SessionInbox) {
        let carried = self.carries_anything();
        if let Some(claim) = self.task_notifications {
            claim.poller.finish_claim(
                &claim.session_id,
                &claim.turn_id,
                ending == TurnEnding::Completed,
            );
        }
        if let Some(escalations) = self.escalations {
            if ending != TurnEnding::Failed {
                escalations
                    .registry
                    .mark_notifications_delivered(&escalations.ids);
            }
        }
        if !carried {
            return;
        }
        match ending {
            TurnEnding::Completed => inbox.task_notifications_settled(),
            TurnEnding::Failed => {
                tracing::warn!("notification turn failed; leaving notifications unacked for retry");
                inbox.retry_task_notifications_later();
            }
            TurnEnding::Cancelled => {}
        }
    }
}

/// The sending end of a session's loop. The loop itself is the shared
/// [`TurnLoop`]; this adds what the terminal and the worker wrap around a
/// turn: the runtime it runs on, the mid-turn queue's guard, and the inbox
/// its deliveries are settled against.
#[derive(Default)]
pub struct SessionLoop {
    turns: TurnLoop,
    inbox: Arc<SessionInbox>,
}

impl SessionLoop {
    pub fn new() -> Self {
        Self::default()
    }

    /// What waits for this session's next turn.
    pub fn inbox(&self) -> &SessionInbox {
        &self.inbox
    }

    /// Hand the loop a turn. Returns the receiver its result arrives on.
    pub fn submit_turn(&self, handle: &Handle, turn: TurnInput) -> oneshot::Receiver<TurnResult> {
        let TurnInput {
            runtime,
            request,
            origin,
            deliveries,
        } = turn;
        // Taken now, not when the loop reaches the turn, so idle writers see
        // a turn as running from the moment it is admitted.
        let turn_guard = runtime.mid_turn_queue.begin_turn();
        let executor = runtime.executor.clone();
        let inbox = self.inbox.clone();
        self.turns.submit_turn(
            handle,
            TurnJob {
                run: Box::pin(async move {
                    let _turn_guard = turn_guard;
                    let _runtime = runtime;
                    executor.execute(request).await
                }),
                kind: origin.diagnostic_kind(),
                settle: Some(Box::new(move |result| {
                    deliveries.settle(TurnEnding::of(result), &inbox);
                })),
                // The terminal and the worker cancel through the prompt's
                // cancel token; a handle they drop leaves the turn running.
                stop_when_abandoned: false,
            },
        )
    }

    /// Hand the loop work that rewrites history (see [`HistoryWork`]).
    pub fn submit_history_work(&self, handle: &Handle, work: HistoryWork) {
        self.turns.submit_history_work(handle, work);
    }
}
