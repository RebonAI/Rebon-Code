use super::*;

/// Phase at which the query loop is asking for attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentPollPhase {
    /// The poll after a completed tool round.
    Regular,
    /// The poll before iteration zero or an otherwise terminal response.
    Eager,
}

/// Complete identity and timing for one attachment poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentPollRequest<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub next_iteration: u64,
    pub phase: AttachmentPollPhase,
}

impl<'a> AttachmentPollRequest<'a> {
    pub const fn new(
        session_id: &'a str,
        turn_id: &'a str,
        next_iteration: u64,
        phase: AttachmentPollPhase,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            next_iteration,
            phase,
        }
    }
}

pub trait AttachmentPoller: Send + Sync {
    /// Produce attachment messages to inject before the next model request.
    /// `next_iteration` is the 0-indexed iteration the engine is about to
    /// start (i.e. one greater than the iteration whose tool results were just
    /// folded into the history). `phase` distinguishes the regular post-tool
    /// poll from the eager checks before iteration zero and terminal delivery.
    ///
    /// Implementations are responsible for any state mutations (clearing
    /// one-shot flags, advancing throttles, recording which skills were
    /// announced). The engine only handles the returned messages.
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage>;

    /// Return volatile context that should be present on the next model
    /// request without being appended to durable conversation history.
    /// Implementations must treat this as a non-draining snapshot: the
    /// query loop may call it before every model request.
    fn transient_context(&self) -> Option<String> {
        None
    }

    /// Turn-scoped transient-context variant. The default preserves
    /// pollers that do not keep per-turn state.
    fn transient_context_for_turn(&self, _turn_id: &str) -> Option<String> {
        self.transient_context()
    }

    /// Query-scoped transient-context variant. The default keeps the
    /// parent session separate from the unique prompt turn.
    fn transient_context_for_query(&self, _session_id: &str, turn_id: &str) -> Option<String> {
        self.transient_context_for_turn(turn_id)
    }

    /// Finish the active query turn. `succeeded` is true only when the
    /// prompt executor produced a final outcome; errors, cancellation,
    /// and dropped executor futures report false.
    ///
    /// Stateful pollers can use this to acknowledge attachments that
    /// were accepted by a successful turn or release them for retry.
    /// Default no-op.
    fn finish_turn(&self, _succeeded: bool) {}

    fn finish_turn_for_turn(&self, _turn_id: &str, succeeded: bool) {
        self.finish_turn(succeeded);
    }

    fn finish_turn_for_query(&self, _session_id: &str, turn_id: &str, succeeded: bool) {
        self.finish_turn_for_turn(turn_id, succeeded);
    }

    /// Take report paths announced by attachments from the most recent
    /// poll. The query loop adds these paths to the current tool context
    /// before the next model request so coordinator Read calls can open
    /// newly completed worker reports. Default returns no paths.
    fn take_coordinator_report_paths(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    fn take_coordinator_report_paths_for_turn(&self, _turn_id: &str) -> Vec<std::path::PathBuf> {
        self.take_coordinator_report_paths()
    }

    fn take_coordinator_report_paths_for_query(
        &self,
        _session_id: &str,
        turn_id: &str,
    ) -> Vec<std::path::PathBuf> {
        self.take_coordinator_report_paths_for_turn(turn_id)
    }

    /// Notify the poller that a task-management tool was invoked at
    /// the given iteration. Used by the `task_reminder` producer to
    /// reset its throttle. Default no-op.
    fn notify_task_tool_used(&self, _iteration: u64) {}

    /// Notify the poller that a plan-mode tool (EnterPlanMode /
    /// ExitPlanMode) completed successfully. The implementation
    /// should update the session's `permission_mode` **synchronously**
    /// so the next `poll()` sees the correct mode. Without this,
    /// `poll()` races against the event-consumer task that would
    /// otherwise call `set_permission_mode`.
    ///
    /// `tool_result` is the JSON output from the tool call. For
    /// ExitPlanMode, the implementation inspects `clearContext` and
    /// `plan` fields to set up a pending context reset. Default no-op.
    fn notify_plan_mode_tool(
        &self,
        _tool_name: &str,
        _succeeded: bool,
        _tool_result: Option<&serde_json::Value>,
    ) {
    }

    /// Take the pending context-reset messages (if any). When
    /// `Some`, `TurnControlPlugin::run` starts a fresh controller drive with
    /// the returned messages (a fresh "Implement the following plan" user
    /// message). Returns `None` if no reset is pending.
    ///
    /// Backs ExitPlanMode's clear-context option: the conversation is
    /// cleared and restarts from the returned messages.
    fn take_context_reset(&self) -> Option<Vec<ApiMessage>> {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn attachment_polling_has_one_contextual_entry_point() {
        let source = include_str!("traits.rs").replace("\r\n", "\n");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let attachment_trait = production
            .split("pub trait AttachmentPoller")
            .nth(1)
            .unwrap()
            .split("#[cfg(test)]")
            .next()
            .unwrap();

        assert!(production.contains("pub struct AttachmentPollRequest<'a>"));
        assert!(production.contains("pub enum AttachmentPollPhase"));
        assert!(attachment_trait
            .contains("fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage>;"));
        for legacy_entry in [
            "fn poll_eager(",
            "fn poll_for_turn(",
            "fn poll_eager_for_turn(",
            "fn poll_for_query(",
            "fn poll_eager_for_query(",
        ] {
            assert!(
                !attachment_trait.contains(legacy_entry),
                "AttachmentPoller still owns legacy entry {legacy_entry}"
            );
        }
        assert_eq!(attachment_trait.matches("fn poll(").count(), 1);
    }
}
