//! What the session is doing, in the closed `SessionRunState` vocabulary.
//!
//! | Owner says | Reported |
//! |---|---|
//! | a pending permission or question, or status `needs_input` | `needs_input` |
//! | `busy`, or status `running` | `running` |
//! | status `queued` | `starting` |
//! | status `idle` or `succeeded` (a hosted session takes more prompts) | `idle` |
//! | status `failed` | `failed`, with the owner's error as detail |
//! | status `stopped` | `stopped` |
//! | turn event `running` | `running` (`needs_input` while a prompt is pending) |
//! | turn event `idle` | `idle` |
//!
//! The runner adds three of its own: `starting` while it opens the
//! session, `stopped` when the session was stopped on the machine, and
//! `failed` when it could not be opened.

use rebon_bridge::session_stream::{SessionFrame, SessionRunState};
use rebon_session_host::{BackgroundJobStatus, SessionStatusSnapshot, TurnStreamState};

/// A state and its detail, as one `session_state` frame carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub state: SessionRunState,
    pub detail: Option<String>,
}

impl Reported {
    pub fn new(state: SessionRunState) -> Self {
        Self {
            state,
            detail: None,
        }
    }

    pub fn with_detail(state: SessionRunState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: Some(detail.into()),
        }
    }

    pub fn frame(&self) -> SessionFrame {
        SessionFrame::SessionState {
            state: self.state.clone(),
            detail: self.detail.clone(),
        }
    }
}

/// The reported state for an owner's status snapshot.
pub fn reported_for_status(status: &SessionStatusSnapshot) -> Reported {
    if status.pending_permission.is_some()
        || status.ask_user_questions.is_some()
        || status.status == BackgroundJobStatus::NeedsInput
    {
        return Reported::new(SessionRunState::NeedsInput);
    }
    if status.busy {
        return Reported::new(SessionRunState::Running);
    }
    match status.status {
        BackgroundJobStatus::Running => Reported::new(SessionRunState::Running),
        BackgroundJobStatus::NeedsInput => Reported::new(SessionRunState::NeedsInput),
        BackgroundJobStatus::Queued => Reported::new(SessionRunState::Starting),
        BackgroundJobStatus::Idle | BackgroundJobStatus::Succeeded => {
            Reported::new(SessionRunState::Idle)
        }
        BackgroundJobStatus::Failed => match &status.last_command_error {
            Some(error) => Reported::with_detail(SessionRunState::Failed, error.clone()),
            None => Reported::new(SessionRunState::Failed),
        },
        BackgroundJobStatus::Stopped => Reported::new(SessionRunState::Stopped),
    }
}

/// The reported state for a turn event.
pub fn reported_for_turn(
    state: TurnStreamState,
    stop_refused: Option<&str>,
    permission_pending: bool,
) -> Reported {
    match state {
        TurnStreamState::Running if permission_pending => {
            Reported::new(SessionRunState::NeedsInput)
        }
        TurnStreamState::Running => match stop_refused {
            Some(reason) => Reported::with_detail(
                SessionRunState::Running,
                format!("the turn did not stop: {reason}"),
            ),
            None => Reported::new(SessionRunState::Running),
        },
        TurnStreamState::Idle => Reported::new(SessionRunState::Idle),
    }
}

/// Sends a state only when it changes.
///
/// Every `session_state` frame is a committed row on the server; an owner
/// republishes its status on every I4 change, most of which move nothing
/// a controller shows.
#[derive(Debug, Default)]
pub struct StateTracker {
    last: Option<Reported>,
}

impl StateTracker {
    /// `Some(frame)` when `next` differs from what was last sent.
    pub fn observe(&mut self, next: Reported) -> Option<SessionFrame> {
        if self.last.as_ref() == Some(&next) {
            return None;
        }
        let frame = next.frame();
        self.last = Some(next);
        Some(frame)
    }

    /// What was last sent, for a reconnect to say again.
    pub fn current(&self) -> Option<SessionFrame> {
        self.last.as_ref().map(Reported::frame)
    }

    pub fn last(&self) -> Option<&Reported> {
        self.last.as_ref()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn status(status: BackgroundJobStatus, busy: bool) -> SessionStatusSnapshot {
        SessionStatusSnapshot {
            job_id: "bg-1".into(),
            session_id: Some("s-1".into()),
            cwd: "/srv/app".into(),
            status,
            busy,
            turn_generation: 1,
            permission_mode: None,
            plan_mode: false,
            model: None,
            effort: None,
            agent: None,
            pending_permission: None,
            ask_user_questions: None,
            usage: None,
            mcp: None,
            client_leases: Vec::new(),
            last_command_id: None,
            last_command_at_ms: 0,
            last_command_error: None,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn every_job_status_has_a_reported_state() {
        use BackgroundJobStatus as S;
        use SessionRunState as R;
        let table = [
            (S::Queued, R::Starting),
            (S::Running, R::Running),
            (S::NeedsInput, R::NeedsInput),
            (S::Idle, R::Idle),
            (S::Succeeded, R::Idle),
            (S::Failed, R::Failed),
            (S::Stopped, R::Stopped),
        ];
        for (job, expected) in table {
            assert_eq!(
                reported_for_status(&status(job, false)).state,
                expected,
                "{job:?}"
            );
        }
    }

    #[test]
    fn busy_and_pending_override_the_job_status() {
        assert_eq!(
            reported_for_status(&status(BackgroundJobStatus::Idle, true)).state,
            SessionRunState::Running
        );
        let mut waiting = status(BackgroundJobStatus::Running, true);
        waiting.pending_permission = Some(crate::core::uplink::tests::query(3, 1));
        assert_eq!(
            reported_for_status(&waiting).state,
            SessionRunState::NeedsInput
        );
        let mut asking = status(BackgroundJobStatus::Running, true);
        asking.ask_user_questions = Some(Vec::new());
        assert_eq!(
            reported_for_status(&asking).state,
            SessionRunState::NeedsInput
        );
    }

    #[test]
    fn a_failure_carries_the_owners_error() {
        let mut failed = status(BackgroundJobStatus::Failed, false);
        failed.last_command_error = Some("provider refused".into());
        assert_eq!(
            reported_for_status(&failed),
            Reported::with_detail(SessionRunState::Failed, "provider refused")
        );
    }

    #[test]
    fn turn_events_map_to_running_and_idle() {
        assert_eq!(
            reported_for_turn(TurnStreamState::Running, None, false),
            Reported::new(SessionRunState::Running)
        );
        assert_eq!(
            reported_for_turn(TurnStreamState::Running, None, true),
            Reported::new(SessionRunState::NeedsInput)
        );
        assert_eq!(
            reported_for_turn(TurnStreamState::Idle, None, true),
            Reported::new(SessionRunState::Idle)
        );
        assert_eq!(
            reported_for_turn(TurnStreamState::Running, Some("hook kept it"), false),
            Reported::with_detail(
                SessionRunState::Running,
                "the turn did not stop: hook kept it"
            )
        );
    }

    #[test]
    fn the_tracker_only_speaks_on_change() {
        let mut tracker = StateTracker::default();
        assert!(tracker.current().is_none());
        let running = Reported::new(SessionRunState::Running);
        assert_eq!(tracker.observe(running.clone()), Some(running.frame()));
        assert_eq!(tracker.observe(running.clone()), None);
        let detailed = Reported::with_detail(SessionRunState::Running, "x");
        assert_eq!(tracker.observe(detailed.clone()), Some(detailed.frame()));
        assert_eq!(tracker.current(), Some(detailed.frame()));
        assert_eq!(tracker.last(), Some(&detailed));
    }
}
