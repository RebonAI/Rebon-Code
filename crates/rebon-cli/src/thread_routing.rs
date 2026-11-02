//! Pull requests as threads: the fast path that tells a session what
//! happened to the pull requests it opened.
//!
//! Run by the automation host on its pass, against the supervisor-owned
//! state file. A job whose summary names a pull request subscribes to it; a
//! thread is looked at with `gh` every few minutes, one per pass; a change
//! of state worth hearing about — checks failed, changes requested, merged,
//! closed — is queued into the subscribed job as a scheduled turn, which
//! starts its worker if it has none. Only program-level filtering: no model
//! decides here.
//!
//! A thread whose job is gone or was stopped has nobody to tell. With a
//! router running it goes to the router instead, which decides who should
//! own it now: that is the slow path. Without one it is dropped, as before.

use rebon_automation::threads::{
    event_prompt, expire_threads, next_thread_to_check, observe_pr, subscribe_prs, take_delivery,
    PrSnapshot, PrThread,
};
use rebon_automation::AutomationState;

use crate::background::{BackgroundJobState, BackgroundJobStatus};

/// Look at a subscribed pull request.
pub(crate) type ObservePr<'a> = dyn FnMut(&PrThread) -> anyhow::Result<PrSnapshot> + 'a;
/// Queue `prompt` as `job_id`'s next turn.
pub(crate) type DeliverToJob<'a> = dyn FnMut(&str, String) -> anyhow::Result<()> + 'a;

/// One routing pass. `router` is the router's job, if one runs. `true` when
/// `state` changed.
pub(crate) fn route_pr_threads(
    state: &mut AutomationState,
    jobs: &[BackgroundJobState],
    router: Option<&str>,
    now_ms: u64,
    observe: &mut ObservePr<'_>,
    deliver: &mut DeliverToJob<'_>,
) -> bool {
    let mut changed = expire_threads(state, now_ms);
    for job in jobs {
        let Some(summary) = job.outcome.summary.as_deref() else {
            continue;
        };
        let prs = crate::background::pull_requests_named_in(summary);
        changed |= subscribe_prs(state, &job.identity.job_id, &job.identity.cwd, &prs, now_ms);
    }
    let Some(key) = next_thread_to_check(state, now_ms) else {
        return changed;
    };
    let thread = state.threads.get_mut(&key).expect("just picked");
    let owner = jobs.iter().find(|job| job.identity.job_id == thread.job_id);
    let owner_stopped = owner.is_some_and(|job| job.process.status == BackgroundJobStatus::Stopped);
    // A thread whose job is gone or stopped goes to the router, if one runs
    // and it is not the router's own. It keeps its last look, so only what
    // happens next is news.
    let router = router.filter(|router| thread.job_id != *router);
    let orphaned_from = match (owner, router) {
        (Some(_), _) if !owner_stopped => None,
        (_, Some(router)) => Some(std::mem::replace(&mut thread.job_id, router.to_string())),
        // Gone, with nobody to decide: nothing is worth a look.
        (None, None) => {
            state.threads.remove(&key);
            return true;
        }
        // Stopped, with nobody to decide: still subscribed, but it hears
        // nothing, since the user stopped it.
        (Some(_), None) => None,
    };
    let deliverable = orphaned_from.is_some() || !owner_stopped;
    let snapshot = match observe(thread) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            // `gh` missing, offline or not logged in: try again next interval.
            tracing::debug!(thread = %key, error = %format!("{error:#}"), "rebon: could not look at a thread");
            thread.checked_at_ms = Some(now_ms);
            return true;
        }
    };
    let Some(event) = observe_pr(thread, snapshot, now_ms) else {
        return true;
    };
    if deliverable && take_delivery(thread, &event, now_ms) {
        let mut prompt = event_prompt(thread, &event);
        if let Some(previous) = orphaned_from {
            prompt = format!(
                "[Its worker {previous} is gone or was stopped, so the thread is yours now. Hand it to a worker with SubscribeThread, or let it be.]\n{prompt}"
            );
        }
        if let Err(error) = deliver(&thread.job_id, prompt) {
            tracing::warn!(thread = %key, error = %format!("{error:#}"), "rebon: could not deliver a thread event");
        }
    }
    if event.ends_thread() {
        state.threads.remove(&key);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_automation::threads::{PrState, MAX_DELIVERIES_PER_HOUR, PR_CHECK_INTERVAL_MS};

    const PR_URL: &str = "https://github.com/RebonAI/Rebon-Code/pull/42";

    fn job(job_id: &str, status: BackgroundJobStatus, summary: Option<&str>) -> BackgroundJobState {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "p".into(),
                std::path::PathBuf::from("/work/repo"),
                crate::background::BackgroundRuntimeFields {
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
            )
            .unwrap();
        job.identity.job_id = job_id.into();
        job.process.status = status;
        job.outcome.summary = summary.map(str::to_string);
        job
    }

    fn snapshot(state: PrState, failed: u32) -> PrSnapshot {
        PrSnapshot {
            state,
            checks_failed: failed,
            checks_pending: 0,
            changes_requested: false,
            checks_summary: (failed > 0).then(|| format!("{failed} check(s) failed")),
        }
    }

    /// Run a pass where `gh` answers `answer` and deliveries are recorded.
    fn pass(
        state: &mut AutomationState,
        jobs: &[BackgroundJobState],
        now_ms: u64,
        answer: PrSnapshot,
        delivered: &mut Vec<(String, String)>,
    ) -> bool {
        let mut observe = |_: &PrThread| Ok(answer.clone());
        let mut deliver = |job_id: &str, prompt: String| {
            delivered.push((job_id.to_string(), prompt));
            Ok(())
        };
        route_pr_threads(state, jobs, None, now_ms, &mut observe, &mut deliver)
    }

    #[test]
    fn a_job_that_opened_a_pr_hears_when_its_checks_fail() {
        let mut state = AutomationState::default();
        let jobs = [job(
            "job-1",
            BackgroundJobStatus::Succeeded,
            Some(&format!("Opened {PR_URL}")),
        )];
        let mut delivered = Vec::new();

        assert!(pass(
            &mut state,
            &jobs,
            0,
            snapshot(PrState::Open, 0),
            &mut delivered
        ));
        assert!(delivered.is_empty(), "green so far");
        assert!(state.threads.contains_key("pr:rebonai/rebon-code#42"));

        pass(
            &mut state,
            &jobs,
            PR_CHECK_INTERVAL_MS,
            snapshot(PrState::Open, 2),
            &mut delivered,
        );
        let [(job_id, prompt)] = delivered.as_slice() else {
            panic!("one delivery: {delivered:?}")
        };
        assert_eq!(job_id, "job-1");
        assert!(
            prompt.starts_with("[Thread pr:rebonai/rebon-code#42]"),
            "{prompt}"
        );
        assert!(prompt.contains("2 check(s) failed"), "{prompt}");
    }

    #[test]
    fn a_thread_is_looked_at_once_per_interval() {
        let mut state = AutomationState::default();
        let jobs = [job("job-1", BackgroundJobStatus::Idle, Some(PR_URL))];
        let mut looks = 0;
        let mut observe = |_: &PrThread| {
            looks += 1;
            Ok(snapshot(PrState::Open, 0))
        };
        let mut deliver = |_: &str, _: String| Ok(());
        route_pr_threads(&mut state, &jobs, None, 0, &mut observe, &mut deliver);
        route_pr_threads(
            &mut state,
            &jobs,
            None,
            PR_CHECK_INTERVAL_MS - 1,
            &mut observe,
            &mut deliver,
        );
        route_pr_threads(
            &mut state,
            &jobs,
            None,
            PR_CHECK_INTERVAL_MS,
            &mut observe,
            &mut deliver,
        );
        assert_eq!(looks, 2);
    }

    #[test]
    fn the_subscription_outlives_the_summary_that_named_it() {
        // Queueing the event clears the job's summary; the thread stays.
        let mut state = AutomationState::default();
        let mut delivered = Vec::new();
        pass(
            &mut state,
            &[job("job-1", BackgroundJobStatus::Idle, Some(PR_URL))],
            0,
            snapshot(PrState::Open, 0),
            &mut delivered,
        );
        let cleared = [job("job-1", BackgroundJobStatus::Queued, None)];
        pass(
            &mut state,
            &cleared,
            PR_CHECK_INTERVAL_MS,
            snapshot(PrState::Open, 1),
            &mut delivered,
        );
        assert_eq!(delivered.len(), 1);
    }

    #[test]
    fn merging_tells_the_job_and_ends_the_thread() {
        let mut state = AutomationState::default();
        let jobs = [job("job-1", BackgroundJobStatus::Succeeded, Some(PR_URL))];
        let mut delivered = Vec::new();
        pass(
            &mut state,
            &jobs,
            0,
            snapshot(PrState::Open, 0),
            &mut delivered,
        );
        pass(
            &mut state,
            &[job("job-1", BackgroundJobStatus::Succeeded, None)],
            PR_CHECK_INTERVAL_MS,
            snapshot(PrState::Merged, 0),
            &mut delivered,
        );
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].1.contains("was merged"));
        assert!(state.threads.is_empty(), "nothing left to watch");
    }

    #[test]
    fn a_stopped_job_is_not_woken_and_a_vanished_one_drops_its_thread() {
        let mut state = AutomationState::default();
        let mut delivered = Vec::new();
        pass(
            &mut state,
            &[job("job-1", BackgroundJobStatus::Stopped, Some(PR_URL))],
            0,
            snapshot(PrState::Open, 1),
            &mut delivered,
        );
        assert!(delivered.is_empty(), "the user stopped it");
        assert_eq!(state.threads.len(), 1, "still subscribed");

        pass(
            &mut state,
            &[],
            PR_CHECK_INTERVAL_MS,
            snapshot(PrState::Open, 3),
            &mut delivered,
        );
        assert!(state.threads.is_empty());
        assert!(delivered.is_empty());
    }

    #[test]
    fn a_fix_that_keeps_failing_is_capped_per_hour() {
        let mut state = AutomationState::default();
        let summary_jobs = [job("job-1", BackgroundJobStatus::Idle, Some(PR_URL))];
        let mut delivered = Vec::new();
        let mut now = 0;
        // Fail, pass, fail, … every interval.
        for round in 0..(2 * MAX_DELIVERIES_PER_HOUR + 2) {
            let failed = if round % 2 == 0 { 1 } else { 0 };
            pass(
                &mut state,
                &summary_jobs,
                now,
                snapshot(PrState::Open, failed),
                &mut delivered,
            );
            now += PR_CHECK_INTERVAL_MS;
        }
        assert_eq!(delivered.len(), MAX_DELIVERIES_PER_HOUR);
    }

    #[test]
    fn a_thread_whose_worker_is_gone_goes_to_the_router() {
        let mut state = AutomationState::default();
        let opener = [job("job-1", BackgroundJobStatus::Idle, Some(PR_URL))];
        let mut delivered = Vec::new();
        pass(
            &mut state,
            &opener,
            0,
            snapshot(PrState::Open, 0),
            &mut delivered,
        );
        let mut stopped = opener.clone();
        stopped[0].process.status = BackgroundJobStatus::Stopped;
        let mut observe = |_: &PrThread| Ok(snapshot(PrState::Merged, 0));
        let mut deliver = |job_id: &str, prompt: String| {
            delivered.push((job_id.to_string(), prompt));
            Ok(())
        };
        route_pr_threads(
            &mut state,
            &stopped,
            Some("router-job"),
            PR_CHECK_INTERVAL_MS,
            &mut observe,
            &mut deliver,
        );
        let [(job_id, prompt)] = delivered.as_slice() else {
            panic!("one delivery: {delivered:?}")
        };
        assert_eq!(job_id, "router-job");
        assert!(
            prompt.contains("Its worker job-1 is gone or was stopped"),
            "{prompt}"
        );
        assert!(state.threads.is_empty(), "merged ends it");
    }

    #[test]
    fn gh_failing_is_retried_next_interval_without_an_event() {
        let mut state = AutomationState::default();
        let jobs = [job("job-1", BackgroundJobStatus::Idle, Some(PR_URL))];
        let mut observe = |_: &PrThread| Err(anyhow::anyhow!("gh: not logged in"));
        let mut delivered = 0;
        let mut deliver = |_: &str, _: String| {
            delivered += 1;
            Ok(())
        };
        assert!(route_pr_threads(
            &mut state,
            &jobs,
            None,
            0,
            &mut observe,
            &mut deliver
        ));
        let thread = &state.threads["pr:rebonai/rebon-code#42"];
        assert_eq!(thread.checked_at_ms, Some(0));
        assert_eq!(thread.last, None, "no baseline from a failed look");
        assert_eq!(delivered, 0);
    }

    #[test]
    fn with_nothing_subscribed_a_pass_changes_nothing() {
        let mut state = AutomationState::default();
        let mut delivered = Vec::new();
        assert!(!pass(
            &mut state,
            &[job("job-1", BackgroundJobStatus::Idle, Some("no links"))],
            0,
            snapshot(PrState::Open, 0),
            &mut delivered
        ));
    }
}
