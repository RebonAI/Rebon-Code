//! The periodic nudge the task tools speak into the model's history.
//!
//! One attachment, `task_reminder`: a reminder that `TaskCreate`/`TaskUpdate`
//! exist, sent only to a session that has both the tools and a task list going
//! stale, and throttled hard enough that a long session sees it once or twice.
//!
//! It lived in `rebon_core::attachments` and ran inside its fixed producer
//! order, last of eight. It runs last here too: the plugin's producer sits on
//! the kernel's `attachment-producers` seat at [`Order::Reminder`], the rung
//! the seat splits *behind* the engine's own session poller. The message a
//! turn sees, its triggers and its position are unchanged.
//!
//! **What stayed behind.** The two task fields on `SessionAttachmentState` —
//! the last iteration a task tool ran, the last iteration this reminder fired
//! — and the two `ServerState` methods that write them are still
//! `rebon-session-state`'s, next to the plan-mode flags. This module is their
//! only reader and their only writer on the attachment path, and it reads them
//! the way the engine did: a snapshot per poll, a delta applied after.
//!
//! **What it reads from outside the record.** Two handles off the binding
//! ([`SessionAttachmentBinding`]), each supplied by the executor:
//!
//! | handle | used for |
//! |---|---|
//! | [`TurnToolkit`] | whether `TaskCreate`/`TaskUpdate` are in this turn's toolkit — without them the producer has nothing to nag about |
//! | [`TurnTaskList`] | which list to render, resolved per poll so a `TeamCreate` mid-turn lands on the team's list |
//!
//! A host that binds neither reads as "no tools, no list", which is the
//! no-op the engine's own defaults gave.
//!
//! Turning the plugin off stops the nudge, which is the point: the tools go
//! off the seat at the same moment, and a reminder to use tools that are not
//! there would be worse than silence.

use std::collections::HashSet;
use std::sync::Arc;

#[cfg(test)]
use rebon_api::ContentBlock as ApiContentBlock;
use rebon_api::{make_meta_user_message, Message as ApiMessage};
use rebon_core::attachment_seat::{SeatAttachmentProducer, SessionAttachmentBinding, TurnTaskList};
use rebon_core::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};
use rebon_session_state::{ServerState, SessionAttachmentState};
use rebon_tool::tasks::{self, Task as StoredTask, TaskListStatus as StoredTaskStatus};

/// Minimum iterations since last TaskCreate/TaskUpdate before a
/// `task_reminder` fires.
pub const TASK_REMINDER_TURNS_SINCE_WRITE: u64 = 10;

/// Minimum iterations between consecutive `task_reminder` injections.
/// Kept deliberately long: each injection is ~650 chars of history the
/// model re-reads on every later request, and transcript audits showed
/// the 10-iteration cadence firing 4-5 times per session with no
/// behaviour change after the first nudge.
pub const TASK_REMINDER_TURNS_BETWEEN_REMINDERS: u64 = 30;

/// Immutable snapshot of everything the producer reads. Copied in by
/// [`TaskAttachmentPoller::poll`] so the producer doesn't hold a lock and so
/// tests can drive it with synthetic values.
#[derive(Debug, Clone)]
pub struct TaskReminderInput {
    /// Snapshot of the session's attachment state. The producer only reads
    /// from this; mutations go through the returned [`TaskReminderStateDelta`].
    pub attachment_state: SessionAttachmentState,
    /// Current tool-round iteration (0-based). Both throttles measure in it.
    pub iteration: u64,
    /// Whether TaskCreate/TaskUpdate are available in the current session.
    /// The producer only fires when task tools are in the toolkit.
    pub has_task_tools: bool,
    /// Snapshot of the current task list formatted for the reminder message.
    /// Empty string when there are no tasks.
    pub task_list_summary: String,
}

/// Side-effect bundle a poll wants applied to the session record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskReminderStateDelta {
    /// When `Some`, record a `task_reminder` at this iteration.
    pub record_task_reminder_iteration: Option<u64>,
}

/// Aggregate result of one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskReminderOutput {
    /// Messages to push, in order.
    pub messages: Vec<ApiMessage>,
    /// State delta to apply to the session record.
    pub state_delta: TaskReminderStateDelta,
}

/// Periodic nudge to remind the model to use TaskCreate/TaskUpdate.
///
/// Fires when **both** of:
/// * `TASK_REMINDER_TURNS_SINCE_WRITE` iterations have passed since
///   the last TaskCreate/TaskUpdate tool use, AND
/// * `TASK_REMINDER_TURNS_BETWEEN_REMINDERS` iterations since the
///   last `task_reminder` injection.
pub fn task_reminder(input: &TaskReminderInput) -> TaskReminderOutput {
    if !input.has_task_tools {
        return TaskReminderOutput::default();
    }
    // Only nag when there is an actual task list going stale. Cold
    // "consider creating tasks" nudges fired every 10 iterations on
    // sessions that never wanted task tracking, adding a recurring
    // ~650-char reminder to history each time for no behaviour change.
    if input.task_list_summary.is_empty() {
        return TaskReminderOutput::default();
    }

    let since_tool = match input.attachment_state.last_task_tool_iteration {
        Some(last) => input.iteration.saturating_sub(last),
        None => input.iteration, // never used task tools
    };
    let since_reminder = match input.attachment_state.last_task_reminder_iteration {
        Some(last) => input.iteration.saturating_sub(last),
        None => input.iteration, // never reminded
    };

    if since_tool < TASK_REMINDER_TURNS_SINCE_WRITE
        || since_reminder < TASK_REMINDER_TURNS_BETWEEN_REMINDERS
    {
        return TaskReminderOutput::default();
    }

    let mut text = "The task tools haven't been used recently. \
        If you're working on tasks that would benefit from tracking progress, \
        consider using TaskCreate to add new tasks and TaskUpdate to update task \
        status (set to in_progress when starting, completed when done). Also \
        consider cleaning up the task list if it has become stale. Only use \
        these if relevant to the current work. This is just a gentle reminder \
        - ignore if not applicable. Make sure that you NEVER mention this \
        reminder to the user\n"
        .to_string();

    if !input.task_list_summary.is_empty() {
        text.push_str("\n\nHere are the existing tasks:\n\n");
        text.push_str(&input.task_list_summary);
    }

    TaskReminderOutput {
        messages: vec![make_meta_user_message(&text)],
        state_delta: TaskReminderStateDelta {
            record_task_reminder_iteration: Some(input.iteration),
        },
    }
}

fn try_live_task_list_summary(task_list_id: &str) -> anyhow::Result<String> {
    let tasks = tasks::list_tasks(task_list_id)?;
    Ok(format_task_list_summary(tasks))
}

fn format_task_list_summary(tasks: Vec<StoredTask>) -> String {
    let visible_tasks: Vec<_> = tasks
        .into_iter()
        .filter(|task| !is_internal_task(task))
        .collect();
    if visible_tasks.is_empty() {
        return String::new();
    }

    let resolved_task_ids: HashSet<String> = visible_tasks
        .iter()
        .filter(|task| task.status == StoredTaskStatus::Completed)
        .map(|task| task.id.clone())
        .collect();

    visible_tasks
        .into_iter()
        .map(|task| {
            let mut line = format!("#{}. [{}] {}", task.id, task.status.as_str(), task.subject);
            if let Some(owner) = task.owner.as_deref().filter(|owner| !owner.is_empty()) {
                line.push_str(&format!(" (@{owner})"));
            }
            let blocked_by: Vec<String> = task
                .blocked_by
                .into_iter()
                .filter(|id| !resolved_task_ids.contains(id))
                .collect();
            if !blocked_by.is_empty() {
                line.push_str(&format!(" [blocked by: {}]", blocked_by.join(", ")));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_internal_task(task: &StoredTask) -> bool {
    task.metadata
        .as_ref()
        .and_then(|metadata| metadata.get("_internal"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// [`AttachmentPoller`] for one session's turn: the record it reads the
/// throttles from, plus the two handles the binding carried.
pub struct TaskAttachmentPoller {
    state: Arc<ServerState>,
    session_id: String,
    /// Fixed for the turn: the toolkit the executor resolved when the turn
    /// started, which is what the engine's sources reported too.
    has_task_tools: bool,
    /// `None` on a host that binds no task list — the summary is then empty
    /// and the producer stays a no-op.
    task_list: Option<Arc<dyn TurnTaskList>>,
}

impl TaskAttachmentPoller {
    pub fn new(
        state: Arc<ServerState>,
        session_id: impl Into<String>,
        has_task_tools: bool,
        task_list: Option<Arc<dyn TurnTaskList>>,
    ) -> Self {
        Self {
            state,
            session_id: session_id.into(),
            has_task_tools,
            task_list,
        }
    }

    /// The list as the reminder would render it. An unreadable store reads as
    /// "no tasks", which keeps a broken task file from nagging.
    fn task_list_summary(&self) -> String {
        let Some(list) = self.task_list.as_ref() else {
            return String::new();
        };
        try_live_task_list_summary(&list.task_list_id()).unwrap_or_default()
    }
}

impl AttachmentPoller for TaskAttachmentPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let next_iteration = request.next_iteration;
        if !self.has_task_tools {
            return Vec::new();
        }
        let Some(record) = self.state.attachment_session_snapshot(&self.session_id) else {
            return Vec::new();
        };

        let input = TaskReminderInput {
            attachment_state: record.attachment_state.clone(),
            iteration: next_iteration,
            has_task_tools: true,
            task_list_summary: self.task_list_summary(),
        };

        let output = task_reminder(&input);

        if let Some(iter) = output.state_delta.record_task_reminder_iteration {
            let _ = self
                .state
                .record_task_reminder_iteration(&self.session_id, iter);
        }

        output.messages
    }

    /// A `TaskCreate`/`TaskUpdate` that just ran resets the "since write"
    /// throttle. Recorded whether or not the tools are in this turn's
    /// toolkit — the record outlives the turn, and a later turn that does
    /// have them must not read a stale iteration.
    fn notify_task_tool_used(&self, iteration: u64) {
        let _ = self
            .state
            .record_task_tool_iteration(&self.session_id, iteration);
    }
}

/// The seat entry: one poller per session, for every session.
///
/// It never declines a turn. Even a session with no task tools has to keep
/// the poller, because `notify_task_tool_used` is how the record learns a
/// task tool ran at all.
pub struct TaskAttachmentProducer;

impl SeatAttachmentProducer for TaskAttachmentProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        let has_task_tools = binding.has_tool(crate::TASK_CREATE_TOOL_NAME)
            || binding.has_tool(crate::TASK_UPDATE_TOOL_NAME);
        Some(Arc::new(TaskAttachmentPoller::new(
            binding.state.clone(),
            binding.session_id.clone(),
            has_task_tools,
            binding.task_list.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(next_iteration: u64) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new(
            "session",
            "turn",
            next_iteration,
            AttachmentPollPhase::Regular,
        )
    }

    fn base_input() -> TaskReminderInput {
        TaskReminderInput {
            attachment_state: SessionAttachmentState::default(),
            iteration: 0,
            has_task_tools: false,
            task_list_summary: String::new(),
        }
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

    #[test]
    fn task_reminder_is_noop_without_task_tools() {
        let mut input = base_input();
        input.has_task_tools = false;
        input.iteration = 100;
        assert!(task_reminder(&input).messages.is_empty());
    }

    #[test]
    fn task_reminder_fires_after_threshold_iterations() {
        let mut input = base_input();
        input.has_task_tools = true;
        input.task_list_summary = "#1. [in_progress] Fix the bug".into();
        input.iteration = TASK_REMINDER_TURNS_BETWEEN_REMINDERS;
        let output = task_reminder(&input);
        assert_eq!(output.messages.len(), 1);
        assert!(only_text(&output.messages).contains("task tools haven't been used recently"));
        assert_eq!(
            output.state_delta.record_task_reminder_iteration,
            Some(TASK_REMINDER_TURNS_BETWEEN_REMINDERS)
        );
    }

    #[test]
    fn task_reminder_skipped_when_tool_used_recently() {
        let mut input = base_input();
        input.has_task_tools = true;
        input.task_list_summary = "#1. [in_progress] Fix the bug".into();
        input.iteration = 45;
        input.attachment_state.last_task_tool_iteration = Some(40);
        // 45 - 40 = 5 < TURNS_SINCE_WRITE (10)
        assert!(task_reminder(&input).messages.is_empty());
    }

    #[test]
    fn task_reminder_skipped_when_reminder_sent_recently() {
        let mut input = base_input();
        input.has_task_tools = true;
        input.task_list_summary = "#1. [in_progress] Fix the bug".into();
        input.iteration = 45;
        input.attachment_state.last_task_tool_iteration = Some(5);
        input.attachment_state.last_task_reminder_iteration = Some(40);
        // 45 - 40 = 5 < TURNS_BETWEEN_REMINDERS (30)
        assert!(task_reminder(&input).messages.is_empty());
    }

    #[test]
    fn task_reminder_fires_when_both_thresholds_exceeded() {
        let mut input = base_input();
        input.has_task_tools = true;
        input.task_list_summary = "#1. [in_progress] Fix the bug".into();
        input.iteration = 40;
        input.attachment_state.last_task_tool_iteration = Some(15);
        input.attachment_state.last_task_reminder_iteration = Some(10);
        // 40 - 15 = 25 >= 10, 40 - 10 = 30 >= 30
        let output = task_reminder(&input);
        assert_eq!(output.messages.len(), 1);
    }

    #[test]
    fn task_reminder_includes_task_list_summary_when_present() {
        let mut input = base_input();
        input.has_task_tools = true;
        input.iteration = TASK_REMINDER_TURNS_BETWEEN_REMINDERS;
        input.task_list_summary = "#1. [in_progress] Fix the bug\n#2. [pending] Write tests".into();

        let text = only_text(&task_reminder(&input).messages);
        assert!(text.contains("Here are the existing tasks:"));
        assert!(text.contains("#1. [in_progress] Fix the bug"));
    }

    #[test]
    fn task_reminder_is_noop_without_existing_tasks() {
        // Cold "consider creating tasks" nudges are gone: with no task
        // list to go stale there is nothing worth a recurring reminder.
        let mut input = base_input();
        input.has_task_tools = true;
        input.iteration = 100;

        assert!(task_reminder(&input).messages.is_empty());
    }

    #[test]
    fn format_task_list_summary_filters_internal_tasks_and_resolved_blockers() {
        let mut internal_metadata = serde_json::Map::new();
        internal_metadata.insert("_internal".into(), serde_json::json!(true));

        let summary = format_task_list_summary(vec![
            StoredTask {
                id: "1".into(),
                subject: "Done task".into(),
                description: String::new(),
                active_form: None,
                owner: Some("agent-a".into()),
                status: StoredTaskStatus::Completed,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                metadata: None,
            },
            StoredTask {
                id: "2".into(),
                subject: "Open task".into(),
                description: String::new(),
                active_form: None,
                owner: None,
                status: StoredTaskStatus::InProgress,
                blocks: Vec::new(),
                blocked_by: vec!["1".into(), "3".into()],
                metadata: None,
            },
            StoredTask {
                id: "3".into(),
                subject: "Hidden task".into(),
                description: String::new(),
                active_form: None,
                owner: None,
                status: StoredTaskStatus::Pending,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                metadata: Some(internal_metadata),
            },
        ]);

        assert!(summary.contains("#1. [completed] Done task (@agent-a)"));
        assert!(summary.contains("#2. [in_progress] Open task [blocked by: 3]"));
        assert!(!summary.contains("Hidden task"));
        assert!(!summary.contains("blocked by: 1"));
    }

    // ── the seat entry ────────────────────────────────────────────

    struct FixedList(&'static str);

    impl TurnTaskList for FixedList {
        fn task_list_id(&self) -> String {
            self.0.to_string()
        }
    }

    struct OnlyTaskTools;

    impl rebon_core::attachment_seat::TurnToolkit for OnlyTaskTools {
        fn has_tool(&self, name: &str) -> bool {
            name == crate::TASK_UPDATE_TOOL_NAME
        }
    }

    fn session() -> (Arc<ServerState>, String) {
        let state = Arc::new(ServerState::new());
        let record = state.create_session("/tmp/tasks".into(), Vec::new());
        let id = record.id.clone();
        (state, id)
    }

    /// A turn whose toolkit has no task tool gets a poller that says
    /// nothing — but still records that a task tool ran, because the
    /// record outlives the turn.
    #[test]
    fn a_turn_without_task_tools_is_silent_but_still_records_tool_use() {
        let (state, id) = session();
        let binding = SessionAttachmentBinding::new(state.clone(), id.clone())
            .with_task_list(Arc::new(FixedList("solo")));
        let poller = TaskAttachmentProducer
            .poller_for_session(&binding)
            .expect("the producer takes every session");

        assert!(poller.poll(request(100)).is_empty());

        poller.notify_task_tool_used(7);
        let stored = state.get_session(&id).expect("session");
        assert_eq!(stored.attachment_state.last_task_tool_iteration, Some(7));
    }

    /// The toolkit handle is what decides whether the nudge is armed, and
    /// `TaskUpdate` alone is enough.
    #[test]
    fn the_toolkit_handle_arms_the_nudge() {
        let (state, id) = session();
        let armed = SessionAttachmentBinding::new(state.clone(), id.clone())
            .with_toolkit(Arc::new(OnlyTaskTools));
        assert!(armed.has_tool(crate::TASK_UPDATE_TOOL_NAME));
        assert!(!armed.has_tool(crate::TASK_CREATE_TOOL_NAME));

        // No task-list handle: nothing to render, so still silent.
        let poller = TaskAttachmentProducer
            .poller_for_session(&armed)
            .expect("the producer takes every session");
        assert!(poller.poll(request(100)).is_empty());
    }

    /// An unknown session id is a no-op rather than a panic.
    #[test]
    fn an_unknown_session_polls_to_nothing() {
        let state = Arc::new(ServerState::new());
        let poller = TaskAttachmentPoller::new(
            state,
            "sess-ghost",
            true,
            Some(Arc::new(FixedList("solo")) as Arc<dyn TurnTaskList>),
        );
        assert!(poller.poll(request(100)).is_empty());
    }
}
