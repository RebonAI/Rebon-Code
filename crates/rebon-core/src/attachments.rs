//! The two attachments the engine still produces, and the fingerprint of one
//! that left.
//!
//! Attachments are the messages pushed into `params.messages` between tool
//! rounds so the model reads what changed while its stream was in flight.
//! There were eight producers here, run in a fixed order by a single
//! session-backed poller. All eight now reach a turn through the kernel's
//! `attachment-producers` seat ([`crate::attachment_seat`]), each at the rung
//! that holds the place it had in that order, so what the model reads is
//! unchanged:
//!
//! | Producer | Rung | Owner |
//! |---|---|---|
//! | `plan_mode_exit`, `plan_mode`, `plan_mode_reentry` | `Transition` | `rebon-plugin-plan-mode` |
//! | [`date_change`] | `DayRoll` | **here** |
//! | `skill_listing` | `Listing` | `rebon-plugin-skill` |
//! | `nested_memory` | `Context` | `rebon-plugin-memory` |
//! | [`runtime_prompts`] | `Prompt` | **here** |
//! | `teammate_mailbox` | `Mailbox` | `rebon-plugin-tasks` |
//! | `task_reminder` | `Reminder` | `rebon-plugin-tasks` |
//!
//! The two that stayed are the two that are nobody's feature: a calendar that
//! rolled over, and a prompt something outside the model loop queued for this
//! session. Their seat entries — [`DateChangeProducer`],
//! [`RuntimePromptsProducer`] — are registered by `core-tools`, so even these
//! reach a turn the same way every other producer does. Nothing in this module
//! is wired into the query loop directly any more.
//!
//! What is left besides them is [`is_skill_listing_text`]: the run loop drops
//! an attachment that replayed history already carries verbatim, and the skill
//! listing is the one that needs it. The recognizer has to be reachable from
//! the engine, and `rebon-plugin-skill` renders from the same two header
//! constants, so the two halves cannot drift.

use std::sync::Arc;

#[cfg(test)]
use rebon_api::ContentBlock as ApiContentBlock;
use rebon_api::{make_meta_user_message, Message as ApiMessage};
use rebon_session_state::{ServerState, SessionAttachmentState};

use crate::attachment_seat::{SeatAttachmentProducer, SessionAttachmentBinding};
use crate::query::{AttachmentPollPhase, AttachmentPollRequest, AttachmentPoller};

// ── date_change ──────────────────────────────────────────────────

/// What one [`date_change`] poll wants done to the session record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DateChangeOutcome {
    /// The message to push, if the date rolled over.
    pub message: Option<ApiMessage>,
    /// When `Some`, set the record's `last_emitted_date` to this.
    pub set_last_emitted_date: Option<String>,
}

/// `date_change` attachment. On the first poll (no last_emitted_date)
/// records the current date without emitting. On subsequent polls,
/// emits a `date_change` user message when the date has rolled over
/// and updates the stored date.
pub fn date_change(state: &SessionAttachmentState, current_date: &str) -> DateChangeOutcome {
    match &state.last_emitted_date {
        None => DateChangeOutcome {
            message: None,
            set_last_emitted_date: Some(current_date.to_string()),
        },
        Some(last) if last == current_date => DateChangeOutcome::default(),
        Some(_) => {
            let content = format!(
                "The date has changed. Today's date is now {current_date}. DO NOT mention this to the user explicitly because they are already aware."
            );
            DateChangeOutcome {
                message: Some(make_meta_user_message(&content)),
                set_last_emitted_date: Some(current_date.to_string()),
            }
        }
    }
}

/// [`AttachmentPoller`] that puts [`date_change`] on the seat.
pub struct DateChangeAttachmentPoller {
    state: Arc<ServerState>,
    session_id: String,
    /// The clock. Injected so a test can roll the date without waiting for
    /// midnight.
    now: Arc<dyn Fn() -> String + Send + Sync>,
}

impl DateChangeAttachmentPoller {
    pub fn new(state: Arc<ServerState>, session_id: impl Into<String>) -> Self {
        Self::with_clock(state, session_id, Arc::new(current_local_iso_date))
    }

    pub fn with_clock(
        state: Arc<ServerState>,
        session_id: impl Into<String>,
        now: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self {
            state,
            session_id: session_id.into(),
            now,
        }
    }
}

impl AttachmentPoller for DateChangeAttachmentPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let Some(record) = self.state.attachment_session_snapshot(&self.session_id) else {
            return Vec::new();
        };
        let outcome = date_change(&record.attachment_state, &(self.now)());
        if let Some(date) = outcome.set_last_emitted_date {
            let _ = self.state.set_last_emitted_date(&self.session_id, date);
        }
        outcome.message.into_iter().collect()
    }
}

/// The seat entry for the date roll. Takes every session: the calendar
/// applies to all of them, and the first poll is what records the baseline.
pub struct DateChangeProducer;

impl SeatAttachmentProducer for DateChangeProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        Some(Arc::new(DateChangeAttachmentPoller::new(
            binding.state.clone(),
            binding.session_id.clone(),
        )))
    }
}

// ── runtime_prompts ──────────────────────────────────────────────

/// What one [`runtime_prompts`] poll wants done to the session record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimePromptsOutcome {
    /// The queued prompts, rendered. One message each, in queue order.
    pub messages: Vec<ApiMessage>,
    /// How many prompts off the front of the queue were consumed.
    pub consume: usize,
}

/// `runtime_prompts` attachment. Replays every prompt something outside the
/// model loop queued on the record — a hook, a background job, a slash command
/// that fired mid-turn — and reports how many of them to drop off the queue.
pub fn runtime_prompts(state: &SessionAttachmentState) -> RuntimePromptsOutcome {
    let prompts = &state.pending_runtime_prompts;
    if prompts.is_empty() {
        return RuntimePromptsOutcome::default();
    }
    RuntimePromptsOutcome {
        messages: prompts
            .iter()
            .map(|prompt| make_meta_user_message(prompt))
            .collect(),
        consume: prompts.len(),
    }
}

/// [`AttachmentPoller`] that puts [`runtime_prompts`] on the seat.
pub struct RuntimePromptsAttachmentPoller {
    state: Arc<ServerState>,
    session_id: String,
}

impl RuntimePromptsAttachmentPoller {
    pub fn new(state: Arc<ServerState>, session_id: impl Into<String>) -> Self {
        Self {
            state,
            session_id: session_id.into(),
        }
    }
}

impl AttachmentPoller for RuntimePromptsAttachmentPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        if request.phase == AttachmentPollPhase::Eager {
            return Vec::new();
        }
        let Some(record) = self.state.attachment_session_snapshot(&self.session_id) else {
            return Vec::new();
        };
        let outcome = runtime_prompts(&record.attachment_state);
        if outcome.consume > 0 {
            let _ = self
                .state
                .consume_runtime_prompts(&self.session_id, outcome.consume);
        }
        outcome.messages
    }
}

/// The seat entry for the queued prompts. Takes every session: any of them
/// can have something queued on the record.
pub struct RuntimePromptsProducer;

impl SeatAttachmentProducer for RuntimePromptsProducer {
    fn poller_for_session(
        &self,
        binding: &SessionAttachmentBinding,
    ) -> Option<Arc<dyn AttachmentPoller>> {
        Some(Arc::new(RuntimePromptsAttachmentPoller::new(
            binding.state.clone(),
            binding.session_id.clone(),
        )))
    }
}

// ── skill_listing's fingerprint ──────────────────────────────────

/// First line of the initial full listing.
pub const SKILL_LISTING_INITIAL_HEADER: &str =
    "The following skills are available via the Skill tool:";
/// First line of a later delta listing.
pub const SKILL_LISTING_DELTA_HEADER: &str =
    "The following skills were just registered and are now available via the Skill tool:";

/// Whether `text` is a rendered skill-listing reminder (either the
/// initial full listing or a later delta), in its `<system-reminder>`
/// envelope or bare. Used by the run loop to skip re-injecting a
/// listing the replayed history already contains verbatim — the
/// listing's delta state lives in in-memory session state, so process
/// restarts and post-compact resets would otherwise repeat it.
pub fn is_skill_listing_text(text: &str) -> bool {
    let body = text.strip_prefix("<system-reminder>\n").unwrap_or(text);
    body.starts_with(SKILL_LISTING_INITIAL_HEADER) || body.starts_with(SKILL_LISTING_DELTA_HEADER)
}

// ── helpers ──────────────────────────────────────────────────────

pub(crate) fn current_local_iso_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn poll_request(next_iteration: u64) -> AttachmentPollRequest<'static> {
        AttachmentPollRequest::new(
            "sess-test",
            "turn-test",
            next_iteration,
            AttachmentPollPhase::Regular,
        )
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

    fn session() -> (Arc<ServerState>, String) {
        let state = Arc::new(ServerState::new());
        let record = state.create_session("/tmp/test".into(), Vec::new());
        let id = record.id.clone();
        (state, id)
    }

    // ── date_change ───────────────────────────────────────────────

    #[test]
    fn current_date_matches_the_system_local_calendar_date() {
        let before = chrono::Local::now().date_naive();
        let actual = current_local_iso_date();
        let after = chrono::Local::now().date_naive();
        let parsed = chrono::NaiveDate::parse_from_str(&actual, "%Y-%m-%d").unwrap();

        assert!(parsed == before || parsed == after);
    }

    #[test]
    fn date_change_records_without_emitting_on_first_poll() {
        let out = date_change(&SessionAttachmentState::default(), "2026-04-11");
        assert!(out.message.is_none());
        assert_eq!(out.set_last_emitted_date.as_deref(), Some("2026-04-11"));
    }

    #[test]
    fn date_change_is_noop_when_same_date() {
        let mut state = SessionAttachmentState::default();
        state.last_emitted_date = Some("2026-04-11".into());
        let out = date_change(&state, "2026-04-11");
        assert_eq!(out, DateChangeOutcome::default());
    }

    #[test]
    fn date_change_emits_when_date_rolled_over() {
        let mut state = SessionAttachmentState::default();
        state.last_emitted_date = Some("2026-04-10".into());
        let out = date_change(&state, "2026-04-11");
        let text = only_text(&out.message.clone().into_iter().collect::<Vec<_>>());
        assert!(text.contains("2026-04-11"));
        assert!(text.contains("DO NOT mention"));
        assert_eq!(out.set_last_emitted_date.as_deref(), Some("2026-04-11"));
    }

    /// The seat poller is the only path the date roll reaches a turn by now.
    /// It has to record the baseline silently on the first poll and speak up
    /// on the first poll of the next day, writing the record both times.
    #[test]
    fn the_date_roll_poller_records_a_baseline_then_announces_the_new_day() {
        let (state, id) = session();
        let today = Arc::new(Mutex::new("2026-04-11".to_string()));

        let clock = today.clone();
        let poller = DateChangeAttachmentPoller::with_clock(
            state.clone(),
            id.clone(),
            Arc::new(move || clock.lock().expect("clock mutex").clone()),
        );

        assert!(
            poller.poll(poll_request(0)).is_empty(),
            "the first poll only records"
        );
        assert_eq!(
            state
                .get_session(&id)
                .expect("session")
                .attachment_state
                .last_emitted_date
                .as_deref(),
            Some("2026-04-11")
        );
        assert!(
            poller.poll(poll_request(1)).is_empty(),
            "same day, nothing to say"
        );

        *today.lock().expect("clock mutex") = "2026-04-12".to_string();
        let rolled = poller.poll(poll_request(2));
        assert_eq!(rolled.len(), 1);
        assert!(only_text(&rolled).contains("2026-04-12"));
        assert_eq!(
            state
                .get_session(&id)
                .expect("session")
                .attachment_state
                .last_emitted_date
                .as_deref(),
            Some("2026-04-12")
        );
    }

    /// An unknown session id is a no-op rather than a panic.
    #[test]
    fn the_date_roll_poller_is_a_no_op_for_an_unknown_session() {
        let poller = DateChangeAttachmentPoller::new(Arc::new(ServerState::new()), "sess-ghost");
        assert!(poller.poll(poll_request(0)).is_empty());
    }

    // ── runtime_prompts ───────────────────────────────────────────

    #[test]
    fn runtime_prompts_emit_once_and_report_consumed_prefix() {
        let mut state = SessionAttachmentState::default();
        state.pending_runtime_prompts = vec![
            "<system-reminder>first fallback</system-reminder>".into(),
            "<system-reminder>second fallback</system-reminder>".into(),
        ];

        let out = runtime_prompts(&state);

        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.consume, 2);
        let text = only_text(&out.messages);
        assert!(text.contains("first fallback"));
        assert!(text.contains("second fallback"));
    }

    #[test]
    fn runtime_prompts_are_a_noop_on_an_empty_queue() {
        let out = runtime_prompts(&SessionAttachmentState::default());
        assert!(out.messages.is_empty());
        assert_eq!(out.consume, 0);
    }

    /// The queue drains through the record, so the second poll is silent.
    #[test]
    fn the_runtime_prompt_poller_drains_the_queue_it_replayed() {
        let (state, id) = session();
        assert!(
            state.enqueue_runtime_prompt(&id, "<system-reminder>queued</system-reminder>".into())
        );

        let poller = RuntimePromptsAttachmentPoller::new(state.clone(), id.clone());
        let first = poller.poll(poll_request(0));
        assert_eq!(first.len(), 1);
        assert!(only_text(&first).contains("queued"));

        assert!(
            poller.poll(poll_request(1)).is_empty(),
            "the queue was consumed"
        );
        assert!(state
            .get_session(&id)
            .expect("session")
            .attachment_state
            .pending_runtime_prompts
            .is_empty());
    }

    #[test]
    fn the_runtime_prompt_poller_is_a_no_op_for_an_unknown_session() {
        let poller =
            RuntimePromptsAttachmentPoller::new(Arc::new(ServerState::new()), "sess-ghost");
        assert!(poller.poll(poll_request(0)).is_empty());
    }

    // ── the skill listing's fingerprint ───────────────────────────

    #[test]
    fn is_skill_listing_text_matches_both_headers_wrapped_or_bare() {
        assert!(is_skill_listing_text(SKILL_LISTING_INITIAL_HEADER));
        assert!(is_skill_listing_text(SKILL_LISTING_DELTA_HEADER));
        assert!(is_skill_listing_text(&format!(
            "<system-reminder>\n{SKILL_LISTING_INITIAL_HEADER}\n\n- commit: x\n</system-reminder>"
        )));
        assert!(!is_skill_listing_text("The date has changed."));
    }
}
