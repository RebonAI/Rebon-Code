//! The session loop: the one place this session's turns run.
//!
//! A turn is one call into the query loop — the model, its tool rounds, the
//! stop hooks and in-turn compaction all happen inside it. What happens
//! *between* turns used to have no owner: every surface (the terminal, ACP,
//! the hosted worker) decided on its own when the next turn starts and ran it
//! itself. This loop is that owner. Producers send a [`SessionInput`]; the
//! loop takes inputs one at a time and runs each to the end before the next,
//! so a session's history has a single writer.
//!
//! Today the terminal sends every turn it starts here, and the loop only
//! runs them. Later steps move the rest of "what runs next" in: task
//! notifications, teammate and channel messages, cron fires, `/compact` and
//! goal continuation become inputs with an origin, instead of branches in the
//! terminal's idle pass. That is also when the loop grows an output stream —
//! once it can start a turn no surface asked for, the surfaces need to hear
//! about it.

use std::any::Any;
use std::sync::{Arc, Mutex};

use rebon_agent_core::{PromptExecutorError, PromptOutcome, PromptRequest};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use crate::mid_turn_queue::QueuedSubmitTurnGuard;
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

/// How a turn ended.
pub type TurnResult = Result<PromptOutcome, PromptExecutorError>;

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

/// Something for the session loop to act on.
pub enum SessionInput {
    /// Run a turn and report how it ended.
    Turn {
        turn: TurnInput,
        /// Taken when the input was sent, not when the loop reaches it, so
        /// idle writers see a turn as running from the moment it is admitted.
        turn_guard: QueuedSubmitTurnGuard,
        reply: oneshot::Sender<TurnResult>,
    },
    /// Rewrite the session's history between turns: a compaction. It runs
    /// alone, like a turn, so history has one writer at a time.
    HistoryWork(HistoryWork),
}

/// Work that rewrites a session's history, and how it shares the loop.
pub struct HistoryWork {
    /// The work. It reports its own progress and result.
    pub work: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    /// Work nobody asked for (an idle compaction) gives way: any input that
    /// arrives while it runs cancels it and goes next, so the person never
    /// waits on it. Work someone asked for (`/compact`) runs to the end,
    /// and a turn sent meanwhile waits its turn.
    pub yields: bool,
}

/// The sending end of a session's loop. The loop task starts on the first
/// input and restarts if the runtime it ran on has gone away.
#[derive(Default)]
pub struct SessionLoop {
    sender: Mutex<Option<mpsc::UnboundedSender<SessionInput>>>,
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
        let (reply, result) = oneshot::channel();
        let turn_guard = turn.runtime.mid_turn_queue.begin_turn();
        self.send(
            handle,
            SessionInput::Turn {
                turn,
                turn_guard,
                reply,
            },
        );
        result
    }

    /// Hand the loop work that rewrites history (see [`HistoryWork`]).
    pub fn submit_history_work(&self, handle: &Handle, work: HistoryWork) {
        self.send(handle, SessionInput::HistoryWork(work));
    }

    fn send(&self, handle: &Handle, input: SessionInput) {
        let mut sender = self.sender.lock().expect("session loop sender poisoned");
        let input = match sender.as_ref() {
            Some(tx) => match tx.send(input) {
                Ok(()) => return,
                // The loop's runtime went away; start it again below.
                Err(mpsc::error::SendError(input)) => input,
            },
            None => input,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        handle.spawn(run_session(rx, self.inbox.clone()));
        // A fresh receiver is alive until this function returns.
        let _ = tx.send(input);
        *sender = Some(tx);
    }
}

/// The loop itself: take the next input, act on it, repeat.
async fn run_session(mut inputs: mpsc::UnboundedReceiver<SessionInput>, inbox: Arc<SessionInbox>) {
    // An input that arrived while yielding work ran, and cancelled it.
    let mut next = None;
    loop {
        let input = match next.take() {
            Some(input) => input,
            None => match inputs.recv().await {
                Some(input) => input,
                None => break,
            },
        };
        match input {
            SessionInput::Turn {
                mut turn,
                turn_guard,
                reply,
            } => {
                let deliveries = std::mem::take(&mut turn.deliveries);
                let result = run_turn(turn, turn_guard).await;
                deliveries.settle(TurnEnding::of(&result), &inbox);
                let _ = reply.send(result);
            }
            SessionInput::HistoryWork(HistoryWork {
                work,
                yields: false,
            }) => work.await,
            SessionInput::HistoryWork(HistoryWork { work, yields: true }) => {
                tokio::select! {
                    () = work => {}
                    arrived = inputs.recv() => match arrived {
                        // Dropping the work cancels it.
                        Some(input) => next = Some(input),
                        None => break,
                    },
                }
            }
        }
    }
}

async fn run_turn(turn: TurnInput, turn_guard: QueuedSubmitTurnGuard) -> TurnResult {
    let TurnInput {
        runtime,
        request,
        origin,
        deliveries: _,
    } = turn;
    let executor = runtime.executor.clone();
    // Its own task, so a turn that panics reports an error instead of
    // taking the loop down with it.
    let worker = tokio::spawn(async move {
        let _turn_guard = turn_guard;
        let _runtime = runtime;
        executor.execute(request).await
    });
    match worker.await {
        Ok(result) => result,
        Err(err) => Err(join_error(origin.diagnostic_kind(), err)),
    }
}

fn join_error(turn_kind: &'static str, err: tokio::task::JoinError) -> PromptExecutorError {
    if err.is_panic() {
        let join_error = err.to_string();
        let panic = panic_payload_message(err.into_panic());
        tracing::error!(
            turn_kind,
            panic = %panic,
            join_error = %join_error,
            "rebon: prompt executor task panicked"
        );
        return PromptExecutorError::Execution(format!(
            "{turn_kind} prompt executor task panicked: {panic}"
        ));
    }
    let message = if err.is_cancelled() {
        format!("{turn_kind} prompt executor task was aborted before reporting a result")
    } else {
        format!("{turn_kind} prompt executor task failed before reporting a result: {err}")
    };
    tracing::error!(turn_kind, error = %message, "rebon: prompt executor task failed");
    PromptExecutorError::Execution(message)
}

fn panic_payload_message(payload: Box<dyn Any + Send + 'static>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
