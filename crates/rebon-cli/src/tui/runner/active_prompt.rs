//! Shared state for an in-flight prompt turn and optional withdrawal metadata.

use crate::session::runtime::SessionRuntime;
use std::sync::Arc;
use std::time::Instant;

use rebon_agent_core::{PromptExecutorError, PromptOutcome};
use rebon_types::PromptCancel;
use rebon_types::PromptPasteContent;
use tokio::sync::oneshot;

use crate::session::submit_payload::SubmitPayload;

/// oneshot receiver holding the final `PromptOutcome` (or error) of
/// an in-flight prompt turn spawned onto the tokio runtime.
pub(super) type PromptResultRx = oneshot::Receiver<Result<PromptOutcome, PromptExecutorError>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalTurnSource {
    DirectUserSubmit,
    Command,
    QueuedFollowUp,
    AutomaticFollowUp,
    IdleContext,
    TaskNotification,
    /// A cron task that fired with no turn running to drain it.
    Scheduled,
    PermissionRetry,
    ForegroundPermissionRetry,
    AgentViewAttachment,
    #[cfg(test)]
    TestOnly,
}

#[derive(Debug, Clone)]
pub(super) enum WithdrawDestination {
    LiveInput {
        text: String,
        cursor_offset: usize,
        image_pastes: Vec<PromptPasteContent>,
    },
    QueuedFront {
        submit: SubmitPayload,
        mode: String,
    },
    Discard,
}

#[derive(Debug, Clone)]
pub(super) struct WithdrawableSubmit {
    pub(super) destination: WithdrawDestination,
    pub(super) transcript_len_before: usize,
    pub(super) transcript_len_after: usize,
    pub(super) user_message_uuid: Option<String>,
}

pub(super) struct ActivePrompt {
    pub(super) rx: PromptResultRx,
    pub(super) cancel: PromptCancel,
    pub(super) pending_task_notification_ids: Vec<rebon_plugin_tasks::runtime::TaskId>,
    pub(super) pending_question_escalation_ids: Vec<rebon_tool::EscalationId>,
    pub(super) withdrawable: Option<WithdrawableSubmit>,
    pub(super) user_message_uuid: Option<String>,
    /// Exact runtime provider/model captured when this turn was submitted.
    /// Completion accounting uses this snapshot even if the session switches
    /// models before the outcome is polled.
    pub(super) usage_model: Option<(String, String)>,
    /// Exact immutable binding this prompt began with. Kept through result
    /// finalization so file-history close and late cleanup cannot hit a newer
    /// session installed while this turn was detached.
    pub(super) runtime: Option<Arc<SessionRuntime>>,
    /// The production entry point that admitted this turn. Synthetic prompt
    /// handles created by isolated tests use `TestOnly`.
    pub(super) source: LocalTurnSource,
    pub(super) reply_started: bool,
    /// Wall-clock instant when this turn was spawned. Used by the
    /// spinner to derive elapsed time for animation frames.
    pub(super) started_at: Instant,
    /// Feeds messages the user types during this turn into the agent
    /// running it. Only spawned for a backend that can take them; the
    /// local engine polls the same queue itself, from inside its own
    /// loop. Aborted when the prompt is dropped — completed,
    /// cancelled, or detached — so it can never outlive its turn.
    steer_pump: Option<SteerPumpHandle>,
}

/// Aborts the steer pump when the turn it belongs to goes away.
pub(super) struct SteerPumpHandle {
    task: tokio::task::JoinHandle<()>,
    poller: Arc<crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller>,
}

impl Drop for SteerPumpHandle {
    fn drop(&mut self) {
        self.task.abort();
        // The abort can drop a steer future mid-await with a message
        // checked out; its delivery report will never arrive, so put
        // whatever is still in flight back in the queue. A report that
        // already landed wins — the poller settles that race on the
        // in-flight entry itself.
        self.poller.reclaim_in_flight();
    }
}

impl SteerPumpHandle {
    pub(super) fn new(
        task: tokio::task::JoinHandle<()>,
        poller: Arc<crate::session::mid_turn_queue::MidTurnQueuedSubmitPoller>,
    ) -> Self {
        Self { task, poller }
    }
}

impl LocalTurnSource {
    /// The origin the session loop records for a turn from this entry point.
    pub(super) fn turn_origin(self) -> crate::session::session_loop::TurnOrigin {
        use crate::session::session_loop::TurnOrigin;
        match self {
            LocalTurnSource::DirectUserSubmit => TurnOrigin::User,
            LocalTurnSource::Command => TurnOrigin::Command,
            LocalTurnSource::QueuedFollowUp => TurnOrigin::Queued,
            LocalTurnSource::AutomaticFollowUp => TurnOrigin::FollowUp,
            LocalTurnSource::IdleContext => TurnOrigin::IdleContext,
            LocalTurnSource::TaskNotification => TurnOrigin::TaskNotification,
            LocalTurnSource::Scheduled => TurnOrigin::Scheduled,
            LocalTurnSource::PermissionRetry | LocalTurnSource::ForegroundPermissionRetry => {
                TurnOrigin::PermissionReplay
            }
            LocalTurnSource::AgentViewAttachment => TurnOrigin::AgentViewAttachment,
            #[cfg(test)]
            LocalTurnSource::TestOnly => TurnOrigin::User,
        }
    }
}

impl ActivePrompt {
    pub(super) fn new(rx: PromptResultRx, cancel: PromptCancel) -> Self {
        Self {
            rx,
            cancel,
            pending_task_notification_ids: Vec::new(),
            pending_question_escalation_ids: Vec::new(),
            withdrawable: None,
            user_message_uuid: None,
            usage_model: None,
            runtime: None,
            source: {
                #[cfg(test)]
                {
                    LocalTurnSource::TestOnly
                }
                #[cfg(not(test))]
                {
                    LocalTurnSource::DirectUserSubmit
                }
            },
            reply_started: false,
            started_at: Instant::now(),
            steer_pump: None,
        }
    }

    pub(super) fn with_task_notifications(
        rx: PromptResultRx,
        cancel: PromptCancel,
        pending_task_notification_ids: Vec<rebon_plugin_tasks::runtime::TaskId>,
        pending_question_escalation_ids: Vec<rebon_tool::EscalationId>,
    ) -> Self {
        Self {
            rx,
            cancel,
            pending_task_notification_ids,
            pending_question_escalation_ids,
            withdrawable: None,
            user_message_uuid: None,
            usage_model: None,
            runtime: None,
            source: {
                #[cfg(test)]
                {
                    LocalTurnSource::TestOnly
                }
                #[cfg(not(test))]
                {
                    LocalTurnSource::DirectUserSubmit
                }
            },
            reply_started: false,
            started_at: Instant::now(),
            steer_pump: None,
        }
    }

    pub(super) fn with_source(mut self, source: LocalTurnSource) -> Self {
        self.source = source;
        self
    }

    pub(super) fn with_runtime(mut self, runtime: Arc<SessionRuntime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Attach the pump that steers mid-turn messages into this turn.
    pub(super) fn with_steer_pump(mut self, pump: Option<SteerPumpHandle>) -> Self {
        self.steer_pump = pump;
        self
    }

    pub(super) fn with_user_message_uuid(mut self, user_message_uuid: Option<String>) -> Self {
        self.user_message_uuid = user_message_uuid;
        self
    }

    pub(super) fn with_usage_model(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.usage_model = Some((provider.into(), model.into()));
        self
    }

    pub(super) fn with_withdrawable(mut self, withdrawable: WithdrawableSubmit) -> Self {
        self.withdrawable = Some(withdrawable);
        self
    }
}
