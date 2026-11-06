//! Progress checks: the router keeps the person posted without being asked.
//!
//! The router's contract has it report each milestone itself: work planned,
//! a task done, stuck or blocked, everything finished. Between milestones a
//! long stretch can pass with the work moving and nothing said, so the
//! supervisor watches for one. While the router has work in hand, has told
//! the person nothing for [`CHECK_AFTER_MS`], and the work has moved since it
//! last did, it gets a progress check: a scheduled turn that asks for a short
//! update if anything worth the person's time changed.
//!
//! When the router last told the person something is noted by the process
//! hosting it (`rebon_session_host::person_report`). The work has moved when
//! one of the router's tasks changed status or went another round, or one of
//! its workers changed state or finished a turn.

use std::cell::RefCell;

use rebon_session_host::{BackgroundJobState, BackgroundJobStatus, BackgroundStore};

/// How long the router may go without telling the person anything while its
/// work moves.
const CHECK_AFTER_MS: u64 = 10 * 60_000;

/// Where `router_job_id`'s tasks stand: whether any is still open, and a line
/// per task that changes whenever the task moves.
pub(crate) type TaskProgress<'a> = dyn Fn(&str) -> (bool, Vec<String>) + 'a;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    router_job_id: String,
    /// When the router last reported, or was last checked on.
    since_ms: u64,
    /// Its last report as noted, to tell a new one.
    report_ms: Option<u64>,
    /// Where the work stood at `since_ms`.
    progress: Vec<String>,
}

pub(crate) struct ProgressChecks {
    mark: RefCell<Option<Mark>>,
}

impl ProgressChecks {
    pub(crate) fn new() -> Self {
        Self {
            mark: RefCell::new(None),
        }
    }

    /// One tick: give the router a progress check when one is due.
    pub(crate) fn tick(
        &self,
        store: &BackgroundStore,
        jobs: &[BackgroundJobState],
        now_ms: u64,
        tasks: &TaskProgress<'_>,
    ) {
        let mut slot = self.mark.borrow_mut();
        let Some(router) = crate::router::current_router(store) else {
            *slot = None;
            return;
        };
        let router_id = router.identity.job_id.as_str();
        let report_ms = rebon_session_host::person_report::last_person_report_ms(store, router_id);
        let (in_hand, progress) = work(router_id, jobs, tasks);
        let Some(silent_ms) = check_due(&mut slot, &router, report_ms, in_hand, progress, now_ms)
        else {
            return;
        };
        let id = format!("u-internal-scheduled-progress-{now_ms}");
        let text = rebon_tool::cron::mark_scheduled_prompt(&check_prompt(silent_ms));
        if let Err(error) =
            rebon_session_host::append_background_scheduled_prompt(store, router_id, id, text)
        {
            tracing::warn!(error = %format!("{error:#}"), "rebon: could not give the router a progress check; retrying");
            // Due again on the next tick.
            if let Some(mark) = slot.as_mut() {
                mark.since_ms = now_ms.saturating_sub(CHECK_AFTER_MS);
                mark.progress.clear();
            }
        }
    }
}

/// Whether the router has work in hand, and where its work stands.
fn work(
    router_id: &str,
    jobs: &[BackgroundJobState],
    tasks: &TaskProgress<'_>,
) -> (bool, Vec<String>) {
    let (open_tasks, mut progress) = tasks(router_id);
    let mut busy_workers = false;
    for job in jobs
        .iter()
        .filter(|job| job.identity.parent_job_id.as_deref() == Some(router_id))
    {
        busy_workers |= matches!(
            job.process.status,
            BackgroundJobStatus::Queued
                | BackgroundJobStatus::Running
                | BackgroundJobStatus::NeedsInput
        );
        progress.push(format!(
            "worker {} {} {}",
            job.identity.job_id,
            job.process.status.as_str(),
            job.process.turn_generation
        ));
    }
    progress.sort();
    (open_tasks || busy_workers, progress)
}

/// Move the mark along and say whether a check is due: `Some` with how long
/// the router has told the person nothing, once it has work in hand, has
/// gone [`CHECK_AFTER_MS`] since its last report or check, the work has moved
/// since, and nothing else is about to give it a turn. A check that goes out
/// is the new mark, so the same stretch of work is not checked on twice.
fn check_due(
    slot: &mut Option<Mark>,
    router: &BackgroundJobState,
    report_ms: Option<u64>,
    in_hand: bool,
    progress: Vec<String>,
    now_ms: u64,
) -> Option<u64> {
    let router_id = router.identity.job_id.as_str();
    let mark = match slot {
        Some(mark) if mark.router_job_id == router_id => mark,
        _ => slot.insert(Mark {
            router_job_id: router_id.to_string(),
            since_ms: report_ms.unwrap_or(now_ms),
            report_ms,
            progress: progress.clone(),
        }),
    };
    if report_ms > mark.report_ms {
        mark.report_ms = report_ms;
        mark.since_ms = report_ms.unwrap_or(now_ms);
        mark.progress = progress.clone();
    }
    let busy = matches!(
        router.process.status,
        BackgroundJobStatus::Queued | BackgroundJobStatus::Running
    ) || router.has_pending_prompts_to_run();
    if !in_hand
        || now_ms.saturating_sub(mark.since_ms) < CHECK_AFTER_MS
        || progress == mark.progress
        || busy
    {
        return None;
    }
    let silent_ms = now_ms.saturating_sub(mark.report_ms.unwrap_or(mark.since_ms));
    mark.since_ms = now_ms;
    mark.progress = progress;
    Some(silent_ms)
}

fn check_prompt(silent_ms: u64) -> String {
    let minutes = (silent_ms / 60_000).max(1);
    format!(
        "[Progress check]\n\
         You have told the person nothing for {minutes} min, and your work has moved since. \
         If anything they would want to know changed (a task finished, started or got stuck), \
         send them one short update with TellPerson: what finished, what is running, what is next. \
         Recall with no id lists your tasks and workers. \
         If nothing worth their time changed, do nothing."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn router(id: &str, status: BackgroundJobStatus) -> BackgroundJobState {
        let mut job = BackgroundJobState::new(
            "/router".into(),
            ".".into(),
            rebon_session_host::BackgroundRuntimeFields {
                provider: None,
                model: None,
                fast_mode: None,
                channels: Vec::new(),
                development_channels: Vec::new(),
                provider_format: None,
                ui_mode: None,
                effort_level: None,
                permission_mode: None,
                capability_mode: rebon_types::AgentCapabilityMode::Normal,
                settings: Vec::new(),
                add_dirs: Vec::new(),
                plugin_dirs: Vec::new(),
                mcp_configs: Vec::new(),
                strict_mcp_config: false,
            },
            Some("router".into()),
        );
        job.identity.job_id = id.into();
        job.process.status = status;
        job
    }

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn a_check_waits_for_silence_and_movement_and_comes_once_per_stretch() {
        let idle = router("router", BackgroundJobStatus::Idle);
        let mut slot = None;
        let start = lines(&["task t-1 running 1 0"]);
        let moved = lines(&["task t-1 verifying 1 0"]);
        // Reported at 0; the mark starts there.
        assert_eq!(check_due(&mut slot, &idle, Some(0), true, start, MIN), None);
        // Moved, but not silent long enough.
        assert_eq!(
            check_due(&mut slot, &idle, Some(0), true, moved.clone(), 5 * MIN),
            None
        );
        // Silent long enough, and moved: a check.
        assert_eq!(
            check_due(&mut slot, &idle, Some(0), true, moved.clone(), 11 * MIN),
            Some(11 * MIN)
        );
        // The same stretch is not checked on again.
        assert_eq!(
            check_due(&mut slot, &idle, Some(0), true, moved, 30 * MIN),
            None
        );
        // Moving again after another quiet stretch earns another.
        let again = lines(&["task t-1 done 1 0"]);
        assert_eq!(
            check_due(&mut slot, &idle, Some(0), true, again, 32 * MIN),
            Some(32 * MIN)
        );
    }

    #[test]
    fn no_check_without_work_in_hand_while_busy_or_soon_after_a_report() {
        let mut slot = None;
        let idle = router("router", BackgroundJobStatus::Idle);
        let start = lines(&["task t-1 running 1 0"]);
        let moved = lines(&["task t-1 verifying 1 0"]);
        assert_eq!(check_due(&mut slot, &idle, None, true, start, 0), None);
        // Nothing in hand: nothing to report on.
        assert_eq!(
            check_due(&mut slot, &idle, None, false, moved.clone(), 20 * MIN),
            None
        );
        // The router is in a turn: it may be reporting right now.
        let running = router("router", BackgroundJobStatus::Running);
        assert_eq!(
            check_due(&mut slot, &running, None, true, moved.clone(), 20 * MIN),
            None
        );
        // It reported at 19 min: counting starts again from there.
        assert_eq!(
            check_due(&mut slot, &idle, Some(19 * MIN), true, moved, 20 * MIN),
            None
        );
        let later = lines(&["task t-1 done 1 0"]);
        assert_eq!(
            check_due(
                &mut slot,
                &idle,
                Some(19 * MIN),
                true,
                later.clone(),
                25 * MIN
            ),
            None
        );
        assert_eq!(
            check_due(&mut slot, &idle, Some(19 * MIN), true, later, 29 * MIN),
            Some(10 * MIN)
        );
    }

    #[test]
    fn a_new_router_starts_counting_afresh() {
        let mut slot = None;
        let first = router("router", BackgroundJobStatus::Idle);
        let start = lines(&["task t-1 running 1 0"]);
        assert_eq!(check_due(&mut slot, &first, None, true, start, 0), None);
        let second = router("router-2", BackgroundJobStatus::Idle);
        let moved = lines(&["task t-2 running 1 0"]);
        assert_eq!(
            check_due(&mut slot, &second, None, true, moved, 15 * MIN),
            None
        );
    }

    #[test]
    fn the_check_asks_for_news_through_tell_person_or_nothing() {
        let prompt = check_prompt(12 * MIN);
        assert!(prompt.starts_with("[Progress check]"));
        assert!(prompt.contains("12 min"));
        assert!(prompt.contains("TellPerson"));
        assert!(prompt.contains("do nothing"));
    }
}
