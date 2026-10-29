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
}

/// The sending end of a session's loop. The loop task starts on the first
/// input and restarts if the runtime it ran on has gone away.
#[derive(Default)]
pub struct SessionLoop {
    sender: Mutex<Option<mpsc::UnboundedSender<SessionInput>>>,
    inbox: SessionInbox,
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
        handle.spawn(run_session(rx));
        // A fresh receiver is alive until this function returns.
        let _ = tx.send(input);
        *sender = Some(tx);
    }
}

/// The loop itself: take the next input, act on it, repeat.
async fn run_session(mut inbox: mpsc::UnboundedReceiver<SessionInput>) {
    while let Some(input) = inbox.recv().await {
        match input {
            SessionInput::Turn {
                turn,
                turn_guard,
                reply,
            } => {
                let result = run_turn(turn, turn_guard).await;
                let _ = reply.send(result);
            }
        }
    }
}

async fn run_turn(turn: TurnInput, turn_guard: QueuedSubmitTurnGuard) -> TurnResult {
    let TurnInput {
        runtime,
        request,
        origin,
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
