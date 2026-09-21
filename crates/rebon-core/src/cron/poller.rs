//! `AttachmentPoller` implementation that forwards cron-scheduled prompts
//! into the model's next iteration.
//!
//! Runtime-state → model injections must flow through the per-iteration
//! poller contract (see `project_attachment_poller` memory) — never through
//! TUI transcript commits. The scheduler enqueues prompts here; `TurnControlPlugin`
//! drains them on the next tick via the single-slot `AttachmentPoller` it
//! already calls each iteration.
//!
//! A single `AttachmentPoller` slot lives on `QueryParams`. When the engine
//! already has another poller (e.g. one off the attachment seat), wrap
//! both in [`CompositePoller`] before handing off to `QueryParams::with_attachment_poller`.

use crate::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};
use rebon_api::Message as ApiMessage;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Queue of pending cron prompts. The scheduler pushes prompts on fire; the
/// next `TurnControlPlugin` iteration drains them and injects each as a user-role
/// message opened by [`rebon_tool::cron::SCHEDULED_PROMPT_MARKER`], so neither
/// the model nor the auto-mode classifier mistakes it for the user typing.
#[derive(Debug, Default)]
pub struct CronPoller {
    queue: Mutex<VecDeque<String>>,
}

impl CronPoller {
    /// Create an empty poller.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Push a prompt onto the queue. Called by the scheduler from its
    /// tokio task on each fire. Cheap — just a mutex + push_back.
    pub fn enqueue(&self, prompt: String) {
        self.queue
            .lock()
            .expect("cron queue poisoned")
            .push_back(prompt);
    }

    /// Non-draining peek at the queue length. Tests only.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.queue.lock().expect("cron queue poisoned").len()
    }
}

impl AttachmentPoller for CronPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let mut queue = self.queue.lock().expect("cron queue poisoned");
        let mut out = Vec::with_capacity(queue.len());
        while let Some(prompt) = queue.pop_front() {
            out.push(ApiMessage::user_text(
                rebon_tool::cron::mark_scheduled_prompt(&prompt),
            ));
        }
        out
    }
}

/// [`AttachmentPoller`] that fans out `poll()` / notifications to two
/// underlying pollers. The engine's `QueryParams` carries a single-slot
/// `Option<Arc<dyn AttachmentPoller>>`; this composite is how we run the
/// cron poller alongside the existing session-state poller without
/// widening that slot.
///
/// Ordering is primary→secondary in `poll`, so attachments from the primary
/// (typically session-state: skill announces, task reminders, etc.) precede
/// cron-injected prompts in the next iteration. `notify_*` and
/// `take_context_reset` forward to both; for `take_context_reset` the first
/// `Some(_)` wins.
pub struct CompositePoller {
    primary: Arc<dyn AttachmentPoller>,
    secondary: Arc<dyn AttachmentPoller>,
}

impl CompositePoller {
    pub fn new(primary: Arc<dyn AttachmentPoller>, secondary: Arc<dyn AttachmentPoller>) -> Self {
        Self { primary, secondary }
    }
}

fn merge_transient_context(primary: Option<String>, secondary: Option<String>) -> Option<String> {
    match (primary, secondary) {
        (Some(primary), Some(secondary)) if !primary.is_empty() && !secondary.is_empty() => {
            Some(format!("{primary}\n\n{secondary}"))
        }
        (Some(primary), _) if !primary.is_empty() => Some(primary),
        (_, Some(secondary)) if !secondary.is_empty() => Some(secondary),
        _ => None,
    }
}

impl AttachmentPoller for CompositePoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        let mut out = self.primary.poll(request);
        out.extend(self.secondary.poll(request));
        out
    }

    fn transient_context(&self) -> Option<String> {
        merge_transient_context(
            self.primary.transient_context(),
            self.secondary.transient_context(),
        )
    }

    fn transient_context_for_turn(&self, turn_id: &str) -> Option<String> {
        merge_transient_context(
            self.primary.transient_context_for_turn(turn_id),
            self.secondary.transient_context_for_turn(turn_id),
        )
    }

    fn transient_context_for_query(&self, session_id: &str, turn_id: &str) -> Option<String> {
        merge_transient_context(
            self.primary
                .transient_context_for_query(session_id, turn_id),
            self.secondary
                .transient_context_for_query(session_id, turn_id),
        )
    }

    fn finish_turn(&self, succeeded: bool) {
        self.primary.finish_turn(succeeded);
        self.secondary.finish_turn(succeeded);
    }

    fn finish_turn_for_turn(&self, turn_id: &str, succeeded: bool) {
        self.primary.finish_turn_for_turn(turn_id, succeeded);
        self.secondary.finish_turn_for_turn(turn_id, succeeded);
    }

    fn finish_turn_for_query(&self, session_id: &str, turn_id: &str, succeeded: bool) {
        self.primary
            .finish_turn_for_query(session_id, turn_id, succeeded);
        self.secondary
            .finish_turn_for_query(session_id, turn_id, succeeded);
    }

    fn take_coordinator_report_paths(&self) -> Vec<std::path::PathBuf> {
        let mut paths = self.primary.take_coordinator_report_paths();
        paths.extend(self.secondary.take_coordinator_report_paths());
        paths
    }

    fn take_coordinator_report_paths_for_turn(&self, turn_id: &str) -> Vec<std::path::PathBuf> {
        let mut paths = self.primary.take_coordinator_report_paths_for_turn(turn_id);
        paths.extend(
            self.secondary
                .take_coordinator_report_paths_for_turn(turn_id),
        );
        paths
    }

    fn take_coordinator_report_paths_for_query(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Vec<std::path::PathBuf> {
        let mut paths = self
            .primary
            .take_coordinator_report_paths_for_query(session_id, turn_id);
        paths.extend(
            self.secondary
                .take_coordinator_report_paths_for_query(session_id, turn_id),
        );
        paths
    }

    fn notify_task_tool_used(&self, iteration: u64) {
        self.primary.notify_task_tool_used(iteration);
        self.secondary.notify_task_tool_used(iteration);
    }

    fn notify_plan_mode_tool(
        &self,
        tool_name: &str,
        succeeded: bool,
        tool_result: Option<&serde_json::Value>,
    ) {
        self.primary
            .notify_plan_mode_tool(tool_name, succeeded, tool_result);
        self.secondary
            .notify_plan_mode_tool(tool_name, succeeded, tool_result);
    }

    fn take_context_reset(&self) -> Option<Vec<ApiMessage>> {
        self.primary
            .take_context_reset()
            .or_else(|| self.secondary.take_context_reset())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{ContentBlock, Role};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn request(next_iteration: u64, phase: AttachmentPollPhase) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new("session", "turn", next_iteration, phase)
    }

    #[test]
    fn poll_drains_queue_as_user_messages() {
        let poller = CronPoller::default();
        poller.enqueue("remind the user to check PRs".into());
        poller.enqueue("run the daily health check".into());
        let msgs = poller.poll(request(1, AttachmentPollPhase::Regular));
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        match &msgs[0].content[0] {
            ContentBlock::Text(tb) => assert_eq!(
                tb.text,
                format!(
                    "{}\nremind the user to check PRs",
                    rebon_tool::cron::SCHEDULED_PROMPT_MARKER
                )
            ),
            other => panic!("expected text block, got {:?}", other),
        }
        // Second poll returns empty.
        assert!(poller
            .poll(request(2, AttachmentPollPhase::Regular))
            .is_empty());
    }

    #[test]
    fn eager_poll_leaves_cron_queue_for_the_regular_round() {
        let poller = CronPoller::default();
        poller.enqueue("queued".into());

        assert!(poller
            .poll(request(0, AttachmentPollPhase::Eager))
            .is_empty());
        assert_eq!(
            poller.poll(request(1, AttachmentPollPhase::Regular)).len(),
            1
        );
    }

    #[test]
    fn composite_forwards_notifications() {
        let counter_a = Arc::new(AtomicU64::new(0));
        let counter_b = Arc::new(AtomicU64::new(0));
        struct Counting(Arc<AtomicU64>);
        impl AttachmentPoller for Counting {
            fn poll(&self, _: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
                Vec::new()
            }
            fn notify_task_tool_used(&self, _: u64) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let composite = CompositePoller::new(
            Arc::new(Counting(counter_a.clone())),
            Arc::new(Counting(counter_b.clone())),
        );
        composite.notify_task_tool_used(42);
        assert_eq!(counter_a.load(Ordering::SeqCst), 1);
        assert_eq!(counter_b.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn composite_forwards_turn_completion() {
        struct Recording(Arc<std::sync::Mutex<Vec<bool>>>);
        impl AttachmentPoller for Recording {
            fn poll(&self, _: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
                Vec::new()
            }
            fn finish_turn(&self, succeeded: bool) {
                self.0.lock().unwrap().push(succeeded);
            }
        }

        let outcomes_a = Arc::new(std::sync::Mutex::new(Vec::new()));
        let outcomes_b = Arc::new(std::sync::Mutex::new(Vec::new()));
        let composite = CompositePoller::new(
            Arc::new(Recording(outcomes_a.clone())),
            Arc::new(Recording(outcomes_b.clone())),
        );

        composite.finish_turn(true);

        assert_eq!(*outcomes_a.lock().unwrap(), vec![true]);
        assert_eq!(*outcomes_b.lock().unwrap(), vec![true]);
    }

    #[test]
    fn composite_merges_coordinator_report_paths() {
        struct Paths(&'static str);
        impl AttachmentPoller for Paths {
            fn poll(&self, _: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
                Vec::new()
            }
            fn take_coordinator_report_paths(&self) -> Vec<std::path::PathBuf> {
                vec![self.0.into()]
            }
        }

        let composite = CompositePoller::new(
            Arc::new(Paths("first.report.md")),
            Arc::new(Paths("second.report.md")),
        );

        assert_eq!(
            composite.take_coordinator_report_paths(),
            vec![
                std::path::PathBuf::from("first.report.md"),
                std::path::PathBuf::from("second.report.md")
            ]
        );
    }

    #[test]
    fn composite_merges_poll_in_order() {
        struct Fixed(&'static str);
        impl AttachmentPoller for Fixed {
            fn poll(&self, _: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
                vec![ApiMessage::user_text(self.0.to_string())]
            }
        }
        let c = CompositePoller::new(Arc::new(Fixed("first")), Arc::new(Fixed("second")));
        let msgs = c.poll(request(0, AttachmentPollPhase::Regular));
        assert_eq!(msgs.len(), 2);
        match &msgs[0].content[0] {
            ContentBlock::Text(tb) => assert_eq!(tb.text, "first"),
            _ => panic!(),
        }
        match &msgs[1].content[0] {
            ContentBlock::Text(tb) => assert_eq!(tb.text, "second"),
            _ => panic!(),
        }
    }

    #[test]
    fn composite_context_reset_prefers_primary() {
        struct WithReset(Option<String>);
        impl AttachmentPoller for WithReset {
            fn poll(&self, _: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
                Vec::new()
            }
            fn take_context_reset(&self) -> Option<Vec<ApiMessage>> {
                self.0.clone().map(|s| vec![ApiMessage::user_text(s)])
            }
        }
        let composite = CompositePoller::new(
            Arc::new(WithReset(Some("primary".into()))),
            Arc::new(WithReset(Some("secondary".into()))),
        );
        let reset = composite.take_context_reset().unwrap();
        match &reset[0].content[0] {
            ContentBlock::Text(tb) => assert_eq!(tb.text, "primary"),
            _ => panic!(),
        }
    }
}
