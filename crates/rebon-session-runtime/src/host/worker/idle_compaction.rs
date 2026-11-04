//! A parked worker compacts its session once before letting it go.
//!
//! The terminal compacts a conversation left idle past the prompt cache
//! (`tui::runner::compact_runtime`). A background worker never did, so the
//! next wake — a pull-request event, a wakeup, a reply hours later — resent
//! the whole history uncached. A worker leaves an hour after its last turn and
//! the supervisor reaps it at the same hour, so waiting the terminal's full
//! hour would race both: the compaction starts a few minutes before the worker
//! would leave instead, and holds it until it ends.
//!
//! It runs on the session's loop as yielding history work, like the
//! terminal's. Anything the worker is about to do with the session — a turn,
//! idle commands, leaving — settles it first: cancelled, or finished and
//! installed, never both racing a session being rebuilt.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use rebon_api::context_prune::ContextBudget;
use rebon_core::query::ResumeReplayHandle;
use rebon_session_host::BackgroundStore;

use crate::compact::{CompactDone, CompactProgress};

/// The terminal's idle delay: past it the prompt cache has expired.
pub const IDLE_COMPACTION_DELAY_MS: u64 = 60 * 60 * 1000;
/// How long before the worker would leave the compaction starts, so it ends
/// before the linger does or the supervisor reaps.
pub const IDLE_COMPACTION_LEAD_MS: u64 = 5 * 60 * 1000;
/// Never sooner than this after a turn: a worker that leaves quickly (a
/// foreground job whose terminal closed) is let go as it is.
pub const IDLE_COMPACTION_MIN_IDLE_MS: u64 = 30 * 60 * 1000;
/// The terminal's floor: a conversation this small is not worth a summary.
pub const IDLE_COMPACTION_MIN_TOKENS: u32 = 8_000;
/// How long settling waits for a cancelled run to let go.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether an idle stretch that began at `idle_since` is due its compaction.
///
/// `leave_at` is when the worker would go — its linger, or the supervisor's
/// reap — and `wakeup_at` a wakeup the session has armed.
pub(crate) fn idle_compaction_due(
    now: u64,
    idle_since: u64,
    leave_at: u64,
    wakeup_at: Option<u64>,
    input_tokens: u32,
    budget_allows: bool,
    attempted_for: Option<u64>,
) -> bool {
    if attempted_for == Some(idle_since)
        || !budget_allows
        || input_tokens < IDLE_COMPACTION_MIN_TOKENS
    {
        return false;
    }
    // A wakeup about to fire runs while the cache is still warm; compacting
    // first would only cost it the history it is about to need.
    if wakeup_at.is_some_and(|at| at < now.saturating_add(IDLE_COMPACTION_LEAD_MS)) {
        return false;
    }
    let before_leaving = leave_at
        .saturating_sub(IDLE_COMPACTION_LEAD_MS)
        .max(idle_since.saturating_add(IDLE_COMPACTION_MIN_IDLE_MS));
    now >= idle_since
        .saturating_add(IDLE_COMPACTION_DELAY_MS)
        .min(before_leaving)
}

/// Whether a session last written at `last_activity_ms` resumes past the
/// prompt cache, so its first request would resend the history uncached.
/// No activity on record says nothing, so it is not due.
pub(crate) fn cold_resume_compaction_due(now: u64, last_activity_ms: Option<u64>) -> bool {
    last_activity_ms.is_some_and(|last| now.saturating_sub(last) >= IDLE_COMPACTION_DELAY_MS)
}

/// Arm one compaction before the first request of a resumed session that has
/// sat idle past the prompt cache.
///
/// The parked worker's own idle compaction covers a worker that stays alive.
/// This covers the rest: a worker that was reaped, crashed or restarted, or
/// whose idle compaction was cancelled or skipped. The transcript's mtime is
/// its last write, and a newer `.compact.json` counts as activity, so a
/// session already compacted while idle is left alone. A recovered turn's
/// saved user row is already in the transcript and only makes this later.
pub(crate) fn arm_cold_resume_compaction(
    store: &BackgroundStore,
    job_id: &str,
    session: &crate::EngineSession,
) {
    if session.engine_half.runtime.session_agents.current_id() != "local"
        || !rebon_config::saved_idle_auto_compact_enabled()
    {
        return;
    }
    // The built session's own path: a resume moves the transcript into the
    // cwd it runs in, worktree or not, before this point.
    let path = rebon_session::transcript_file_path(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    );
    let last_activity_ms =
        rebon_group::warmth::rebon_transcript_activity(&path, &session.session_id).last_activity_ms;
    let now = rebon_types::wall_clock_ms();
    if !cold_resume_compaction_due(now, last_activity_ms) {
        return;
    }
    session
        .model
        .prune_level
        .budget
        .compact_once_if_at_least(IDLE_COMPACTION_MIN_TOKENS);
    let _ = store.append_event(
        job_id,
        "cold_resume_compaction_armed",
        serde_json::json!({ "idleMs": last_activity_ms.map_or(0, |last| now.saturating_sub(last)) }),
    );
}

/// What a parked worker keeps to compact the session it released.
pub struct HeldIdleCompaction {
    pub(crate) session_id: String,
    cwd: String,
    projects_root: PathBuf,
    resume_replay: ResumeReplayHandle,
    budget: Arc<ContextBudget>,
    /// The idle stretch already tried, by when it began.
    attempted_for: Option<u64>,
    running: Option<RunningCompaction>,
}

struct RunningCompaction {
    progress: Receiver<CompactProgress>,
    /// Dropping it cancels the run.
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    tokens_before: u32,
}

impl HeldIdleCompaction {
    /// Capture `session` as it is released. A release of the same session
    /// keeps the stretch already tried, so idle commands run against a
    /// rebuilt session do not buy it a second attempt.
    pub(crate) fn of(session: &crate::EngineSession, previous: Option<Self>) -> Option<Self> {
        if session.engine_half.runtime.session_agents.current_id() != "local" {
            return None;
        }
        let attempted_for = previous
            .filter(|previous| previous.session_id == session.session_id)
            .and_then(|previous| previous.attempted_for);
        Some(Self {
            session_id: session.session_id.clone(),
            cwd: session.cwd.clone(),
            projects_root: session.projects_root.clone(),
            resume_replay: session.engine_half.resume_replay(),
            budget: Arc::clone(&session.model.prune_level.budget),
            attempted_for,
            running: None,
        })
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// One idle tick: collect a run that ended, or start one that is due.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tick(
        &mut self,
        store: &BackgroundStore,
        job_id: &str,
        session_loop: Option<&crate::session_loop::SessionLoop>,
        now: u64,
        idle_since: u64,
        leave_at: u64,
        wakeup_at: Option<u64>,
    ) {
        if self.running.is_some() {
            self.collect(store, job_id);
            return;
        }
        let Some(session_loop) = session_loop else {
            return;
        };
        let budget = &self.budget;
        if !idle_compaction_due(
            now,
            idle_since,
            leave_at,
            wakeup_at,
            budget.last_input_tokens(),
            budget.should_auto_compact_for_tokens(budget.auto_compact_threshold()),
            self.attempted_for,
        ) {
            return;
        }
        // One try per stretch, whether it runs or the setting is off.
        self.attempted_for = Some(idle_since);
        if !rebon_config::saved_idle_auto_compact_enabled() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
        let compact = crate::compact::compact_and_install(
            self.resume_replay.clone(),
            self.projects_root.clone(),
            self.cwd.clone(),
            self.session_id.clone(),
            None,
            true,
            tx,
        );
        // Either a send or a dropped sender ends the wait, so letting go of
        // `cancel` is enough to stop it.
        let work = async move {
            tokio::select! {
                () = compact => {}
                _ = cancelled => {}
            }
        };
        session_loop.submit_history_work(
            &tokio::runtime::Handle::current(),
            crate::session_loop::HistoryWork {
                work: Box::pin(work),
                yields: true,
            },
        );
        self.running = Some(RunningCompaction {
            progress: rx,
            cancel: Some(cancel),
            tokens_before: budget.last_input_tokens(),
        });
        let _ = store.append_event(
            job_id,
            "idle_compaction_started",
            serde_json::json!({ "tokens": budget.last_input_tokens() }),
        );
    }

    /// Stop a run before the worker does anything else with the session,
    /// and wait until it has let go. One that already finished is collected.
    pub(crate) async fn settle(&mut self, store: &BackgroundStore, job_id: &str) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        drop(running.cancel.take());
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        while self.running.is_some() {
            self.collect(store, job_id);
            if self.running.is_none() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(%job_id, "rebon: idle compaction did not stop in time");
                self.running = None;
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn collect(&mut self, store: &BackgroundStore, job_id: &str) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        let done = loop {
            match running.progress.try_recv() {
                Ok(CompactProgress::Summarizing) => {}
                Ok(CompactProgress::Done(done)) => break Some(*done),
                Err(TryRecvError::Empty) => return,
                // Cancelled — by a turn that took the loop, or by settling.
                Err(TryRecvError::Disconnected) => break None,
            }
        };
        let tokens_before = running.tokens_before;
        self.running = None;
        match done {
            Some(CompactDone::Installed(report)) => {
                self.budget.report_estimated_usage(report.tokens_after);
                self.budget.record_compact_success();
                let _ = store.append_event(
                    job_id,
                    "idle_compacted",
                    serde_json::json!({
                        "tokensBefore": report.tokens_before,
                        "tokensAfter": report.tokens_after,
                        "messagesBefore": report.messages_before,
                        "messagesAfter": report.messages_after,
                    }),
                );
            }
            Some(CompactDone::NoSummary(_)) => {
                self.failed(store, job_id, "no summary was written", tokens_before)
            }
            Some(CompactDone::Unverified(error)) | Some(CompactDone::Failed(error)) => {
                self.failed(store, job_id, &error, tokens_before)
            }
            Some(CompactDone::Stale) | None => {}
        }
    }

    fn failed(&self, store: &BackgroundStore, job_id: &str, reason: &str, tokens: u32) {
        self.budget.record_compact_failure();
        let _ = store.append_event(
            job_id,
            "idle_compaction_failed",
            serde_json::json!({ "error": reason, "tokens": tokens }),
        );
    }
}

/// When a parked worker would go: its linger, or the supervisor's reap of a
/// worker nobody holds a lease on, whichever comes first.
pub(crate) fn worker_leave_at(
    state: &rebon_session_host::BackgroundJobState,
    now: u64,
    idle_since: u64,
) -> u64 {
    let linger = state.linger_deadline_ms(idle_since);
    if state.has_live_client_lease(now) {
        linger
    } else {
        linger.min(idle_since.saturating_add(rebon_session_host::TERMINAL_WORKER_IDLE_TTL_MS))
    }
}

/// The armed wakeup's fire time, in the store's milliseconds.
pub(crate) fn armed_wakeup_at(store: &rebon_tool::SessionCronStore) -> Option<u64> {
    store
        .wakeup()
        .map(|wakeup| u64::try_from(wakeup.fire_at).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60 * 1000;
    const HOUR: u64 = 60 * MIN;
    const T0: u64 = 1_000_000_000;

    fn due(now: u64, leave_at: u64) -> bool {
        idle_compaction_due(now, T0, leave_at, None, 50_000, true, None)
    }

    #[test]
    fn a_background_worker_compacts_a_few_minutes_before_its_hour_is_up() {
        let leave_at = T0 + HOUR;
        assert!(!due(T0 + 54 * MIN, leave_at));
        assert!(due(T0 + 55 * MIN, leave_at));
    }

    #[test]
    fn a_worker_someone_holds_waits_the_terminals_full_hour() {
        let leave_at = T0 + 5 * HOUR;
        assert!(!due(T0 + 59 * MIN, leave_at));
        assert!(due(T0 + HOUR, leave_at));
    }

    #[test]
    fn a_worker_that_leaves_quickly_is_let_go_uncompacted() {
        // A foreground job's ten-minute linger ends long before the floor.
        let leave_at = T0 + 10 * MIN;
        assert!(!due(T0 + 10 * MIN, leave_at));
        assert!(!due(T0 + 29 * MIN, leave_at));
    }

    #[test]
    fn one_attempt_per_idle_stretch() {
        let now = T0 + HOUR;
        assert!(!idle_compaction_due(
            now,
            T0,
            now,
            None,
            50_000,
            true,
            Some(T0)
        ));
        // A new turn starts a new stretch.
        assert!(idle_compaction_due(
            now + 2 * HOUR,
            T0 + HOUR,
            now + 2 * HOUR,
            None,
            50_000,
            true,
            Some(T0)
        ));
    }

    #[test]
    fn small_conversations_and_a_tripped_budget_are_left_alone() {
        let now = T0 + HOUR;
        assert!(!idle_compaction_due(now, T0, now, None, 7_999, true, None));
        assert!(!idle_compaction_due(
            now, T0, now, None, 50_000, false, None
        ));
    }

    #[test]
    fn a_cold_resume_compacts_once_the_cache_hour_has_passed() {
        assert!(!cold_resume_compaction_due(T0 + HOUR - 1, Some(T0)));
        assert!(cold_resume_compaction_due(T0 + HOUR, Some(T0)));
        assert!(cold_resume_compaction_due(T0 + 5 * HOUR, Some(T0)));
    }

    #[test]
    fn a_cold_resume_without_activity_or_from_the_future_is_left_alone() {
        assert!(!cold_resume_compaction_due(T0 + 5 * HOUR, None));
        // A clock that went backwards reads as no idle time at all.
        assert!(!cold_resume_compaction_due(T0, Some(T0 + HOUR)));
    }

    #[test]
    fn a_wakeup_about_to_fire_keeps_the_history_it_needs() {
        let now = T0 + HOUR;
        let soon = Some(now + 2 * MIN);
        let later = Some(now + 30 * MIN);
        assert!(!idle_compaction_due(now, T0, now, soon, 50_000, true, None));
        assert!(idle_compaction_due(now, T0, now, later, 50_000, true, None));
    }
}
