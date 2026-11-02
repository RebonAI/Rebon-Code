//! Threads: a run of related events a session subscribes to, so the events
//! that follow reach that session instead of a new one (the fast path of
//! DeepSeek Harness's Super-agent routing).
//!
//! The one thread kind so far is a GitHub pull request a session opened: it
//! is subscribed to the job that opened it, and when its checks fail, a
//! reviewer asks for changes, or it is merged or closed, the job is told.
//! Everything here is pure; querying GitHub and queueing the prompt are the
//! caller's, so the rules can be tested without either.

use serde::{Deserialize, Serialize};

use crate::AutomationState;

/// How often a subscribed pull request is looked at again.
pub const PR_CHECK_INTERVAL_MS: u64 = 5 * 60 * 1000;
/// How many events one thread may deliver in an hour: enough to answer a
/// failure and its retry, few enough that a fix that keeps failing cannot
/// keep its session spinning.
pub const MAX_DELIVERIES_PER_HOUR: usize = 3;
/// A thread nobody resolved in this long is dropped: the pull request is
/// stale, and the session that opened it has long moved on.
pub const THREAD_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

const HOUR_MS: u64 = 60 * 60 * 1000;

/// Which pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
}

impl PrRef {
    /// The thread key: `pr:owner/repo#number`, case-folded.
    pub fn key(&self) -> String {
        format!(
            "pr:{}/{}#{}",
            self.owner.to_ascii_lowercase(),
            self.repo.to_ascii_lowercase(),
            self.number
        )
    }
}

/// Whether the pull request is still open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// One look at a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub state: PrState,
    pub checks_failed: u32,
    pub checks_pending: u32,
    pub changes_requested: bool,
    /// The checks' one-line summary, for the prompt.
    #[serde(default)]
    pub checks_summary: Option<String>,
}

/// A subscribed pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrThread {
    pub pr: PrRef,
    /// The job whose session opened it, and is told what happens to it.
    pub job_id: String,
    /// Where `gh` runs for it: the job's project.
    pub cwd: String,
    pub subscribed_at_ms: u64,
    #[serde(default)]
    pub checked_at_ms: Option<u64>,
    /// The last look, the baseline the next one is compared with.
    #[serde(default)]
    pub last: Option<PrSnapshot>,
    /// When events were delivered, kept for the last hour.
    #[serde(default)]
    pub deliveries_ms: Vec<u64>,
}

/// What happened to a pull request that its session should hear about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrEvent {
    /// Every check has finished and at least one failed.
    ChecksFailed {
        summary: Option<String>,
    },
    ChangesRequested,
    Merged,
    Closed,
}

impl PrEvent {
    /// The last event a thread delivers: after it, nothing is left to watch.
    pub fn ends_thread(&self) -> bool {
        matches!(self, Self::Merged | Self::Closed)
    }
}

/// Subscribe `job_id` to the pull requests `prs` its session opened. A pull
/// request that already has a subscriber keeps it. `true` when a thread was
/// added.
pub fn subscribe_prs(
    state: &mut AutomationState,
    job_id: &str,
    cwd: &str,
    prs: &[PrRef],
    now_ms: u64,
) -> bool {
    let mut added = false;
    for pr in prs {
        state.threads.entry(pr.key()).or_insert_with(|| {
            added = true;
            PrThread {
                pr: pr.clone(),
                job_id: job_id.to_string(),
                cwd: cwd.to_string(),
                subscribed_at_ms: now_ms,
                checked_at_ms: None,
                last: None,
                deliveries_ms: Vec::new(),
            }
        });
    }
    added
}

/// Drop threads older than [`THREAD_TTL_MS`]. `true` when any went.
pub fn expire_threads(state: &mut AutomationState, now_ms: u64) -> bool {
    let before = state.threads.len();
    state
        .threads
        .retain(|_, thread| now_ms.saturating_sub(thread.subscribed_at_ms) < THREAD_TTL_MS);
    state.threads.len() != before
}

/// The thread to look at next: the one checked longest ago, among those
/// not checked within [`PR_CHECK_INTERVAL_MS`]. A never-checked one first.
pub fn next_thread_to_check(state: &AutomationState, now_ms: u64) -> Option<String> {
    state
        .threads
        .iter()
        .filter(|(_, thread)| {
            thread
                .checked_at_ms
                .is_none_or(|at| now_ms.saturating_sub(at) >= PR_CHECK_INTERVAL_MS)
        })
        .min_by_key(|(_, thread)| thread.checked_at_ms.unwrap_or(0))
        .map(|(key, _)| key.clone())
}

/// Record a look at `thread` and say what changed since the last one.
///
/// Program-level filtering only: an event is a change of state, never a
/// state seen again. Checks still running say nothing yet. The first look
/// has no baseline, so it reports only a settled failure — the pull request
/// was opened moments ago, and its failing checks are news.
pub fn observe_pr(thread: &mut PrThread, now: PrSnapshot, now_ms: u64) -> Option<PrEvent> {
    let previous = thread.last.replace(now.clone());
    thread.checked_at_ms = Some(now_ms);
    let was_open = previous
        .as_ref()
        .is_none_or(|last| last.state == PrState::Open);
    match now.state {
        PrState::Merged if was_open => return Some(PrEvent::Merged),
        PrState::Closed if was_open => return Some(PrEvent::Closed),
        PrState::Merged | PrState::Closed => return None,
        PrState::Open => {}
    }
    let failing_now = now.checks_failed > 0 && now.checks_pending == 0;
    let failing_before = previous
        .as_ref()
        .is_some_and(|last| last.checks_failed > 0 && last.checks_pending == 0);
    if failing_now && !failing_before {
        return Some(PrEvent::ChecksFailed {
            summary: now.checks_summary.clone(),
        });
    }
    let asked_before = previous.as_ref().is_some_and(|last| last.changes_requested);
    if now.changes_requested && !asked_before {
        return Some(PrEvent::ChangesRequested);
    }
    None
}

/// Take one of the thread's deliveries for the hour, if one is left. The
/// last event of a thread is always delivered: it ends the subscription.
pub fn take_delivery(thread: &mut PrThread, event: &PrEvent, now_ms: u64) -> bool {
    thread
        .deliveries_ms
        .retain(|at| now_ms.saturating_sub(*at) < HOUR_MS);
    if !event.ends_thread() && thread.deliveries_ms.len() >= MAX_DELIVERIES_PER_HOUR {
        return false;
    }
    thread.deliveries_ms.push(now_ms);
    true
}

/// What the subscribed session is told.
pub fn event_prompt(thread: &PrThread, event: &PrEvent) -> String {
    let pr = format!(
        "pull request {}/{}#{} ({})",
        thread.pr.owner, thread.pr.repo, thread.pr.number, thread.pr.url
    );
    let body = match event {
        PrEvent::ChecksFailed { summary } => format!(
            "The checks on {pr} have finished and failed{}. Look at the failing checks \
             (`gh pr checks {}`), fix the cause, and push the fix to the same branch. If the \
             failure is not caused by this change, say so instead of changing code.",
            summary
                .as_deref()
                .map(|summary| format!(" ({summary})"))
                .unwrap_or_default(),
            thread.pr.number
        ),
        PrEvent::ChangesRequested => format!(
            "A reviewer requested changes on {pr}. Read the review (`gh pr view {} --comments`), \
             address it, and push to the same branch.",
            thread.pr.number
        ),
        PrEvent::Merged => format!(
            "{} was merged. Nothing to do unless follow-up work was promised; this thread is \
             closed.",
            capitalize(&pr)
        ),
        PrEvent::Closed => format!(
            "{} was closed without merging. Nothing to do unless the user asked for it to be \
             reopened; this thread is closed.",
            capitalize(&pr)
        ),
    };
    format!("[Thread {}] {body}", thread.pr.key())
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Where the router's requests to hand a thread to a worker wait for the
/// supervisor, which is the only writer of the threads.
pub const SUBSCRIPTION_REQUESTS_DIR_NAME: &str = "thread-requests";

/// A request that `job_id` own the thread of `pr` from now on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionRequest {
    pub pr: PrRef,
    pub job_id: String,
    /// Where `gh` runs for it: the subscriber's project.
    pub cwd: String,
    pub requested_at_ms: u64,
}

pub fn subscription_requests_dir(config_home: &std::path::Path) -> std::path::PathBuf {
    config_home.join(SUBSCRIPTION_REQUESTS_DIR_NAME)
}

/// File a request for the supervisor's next pass.
pub fn request_subscription(
    config_home: &std::path::Path,
    request: &SubscriptionRequest,
) -> anyhow::Result<()> {
    let dir = subscription_requests_dir(config_home);
    std::fs::create_dir_all(&dir)?;
    let name: String = request
        .pr
        .key()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let path = dir.join(format!("{}-{name}.json", request.requested_at_ms));
    rebon_session::write_file_atomically(&path, &serde_json::to_vec_pretty(request)?)?;
    Ok(())
}

/// Take every pending request, oldest first, removing each so it applies
/// once. One that cannot be read is dropped.
pub fn take_subscription_requests(dir: &std::path::Path) -> Vec<SubscriptionRequest> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let request = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            let _ = std::fs::remove_file(&path);
            request
        })
        .collect()
}

/// Apply a request: the thread is the requested job's from now on. A thread
/// already watched keeps its last look, so handing it over does not report
/// its current state again as news. `true` when anything changed.
pub fn apply_subscription(state: &mut AutomationState, request: &SubscriptionRequest) -> bool {
    match state.threads.get_mut(&request.pr.key()) {
        Some(thread) if thread.job_id == request.job_id && thread.cwd == request.cwd => false,
        Some(thread) => {
            thread.job_id = request.job_id.clone();
            thread.cwd = request.cwd.clone();
            thread.subscribed_at_ms = request.requested_at_ms;
            thread.deliveries_ms.clear();
            true
        }
        None => subscribe_prs(
            state,
            &request.job_id,
            &request.cwd,
            std::slice::from_ref(&request.pr),
            request.requested_at_ms,
        ),
    }
}

/// The pull request a thread id names: `pr:owner/repo#12`, or a GitHub pull
/// request URL.
pub fn parse_thread_ref(text: &str) -> Option<PrRef> {
    let text = text.trim();
    if let Some(rest) = text
        .strip_prefix("pr:")
        .or_else(|| text.strip_prefix("PR:"))
    {
        let (path, number) = rest.split_once('#')?;
        let (owner, repo) = path.split_once('/')?;
        return pr_ref(owner, repo, number);
    }
    let rest = text
        .strip_prefix("https://github.com/")
        .or_else(|| text.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let (owner, repo, kind, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if kind != "pull" {
        return None;
    }
    let number = number.split(['?', '#']).next()?;
    pr_ref(owner, repo, number)
}

fn pr_ref(owner: &str, repo: &str, number: &str) -> Option<PrRef> {
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    };
    let number: u64 = number.trim().parse().ok()?;
    (valid(owner) && valid(repo) && number > 0).then(|| PrRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
    })
}

#[cfg(test)]
mod subscription_request_tests {
    use super::*;

    #[test]
    fn a_thread_id_or_a_url_names_a_pull_request() {
        let pr = parse_thread_ref("pr:Acme/widgets#12").unwrap();
        assert_eq!(pr.key(), "pr:acme/widgets#12");
        assert_eq!(pr.url, "https://github.com/Acme/widgets/pull/12");
        assert_eq!(
            parse_thread_ref("https://github.com/acme/widgets/pull/12/files?x=1")
                .unwrap()
                .key(),
            "pr:acme/widgets#12"
        );
        for bad in [
            "pr:acme#12",
            "pr:acme/widgets#x",
            "https://github.com/acme/widgets/issues/3",
            "12",
        ] {
            assert!(parse_thread_ref(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_request_hands_a_thread_over_without_reporting_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let pr = parse_thread_ref("pr:acme/widgets#12").unwrap();
        let mut state = AutomationState::default();
        subscribe_prs(&mut state, "opener", "/p", std::slice::from_ref(&pr), 1);
        let last = PrSnapshot {
            state: PrState::Open,
            checks_failed: 1,
            checks_pending: 0,
            changes_requested: false,
            checks_summary: None,
        };
        state.threads.get_mut(&pr.key()).unwrap().last = Some(last.clone());

        let request = SubscriptionRequest {
            pr: pr.clone(),
            job_id: "fixer".into(),
            cwd: "/wt".into(),
            requested_at_ms: 5,
        };
        request_subscription(dir.path(), &request).unwrap();
        let taken = take_subscription_requests(&subscription_requests_dir(dir.path()));
        assert_eq!(taken, [request.clone()]);
        assert!(take_subscription_requests(&subscription_requests_dir(dir.path())).is_empty());

        assert!(apply_subscription(&mut state, &request));
        let thread = &state.threads[&pr.key()];
        assert_eq!(thread.job_id, "fixer");
        assert_eq!(thread.cwd, "/wt");
        assert_eq!(thread.last, Some(last));
        assert!(!apply_subscription(&mut state, &request), "already theirs");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64) -> PrRef {
        PrRef {
            owner: "RebonAI".into(),
            repo: "Rebon-Code".into(),
            number,
            url: format!("https://github.com/RebonAI/Rebon-Code/pull/{number}"),
        }
    }

    fn open(failed: u32, pending: u32, changes_requested: bool) -> PrSnapshot {
        PrSnapshot {
            state: PrState::Open,
            checks_failed: failed,
            checks_pending: pending,
            changes_requested,
            checks_summary: (failed > 0).then(|| format!("{failed} check(s) failed")),
        }
    }

    fn closed(state: PrState) -> PrSnapshot {
        PrSnapshot {
            state,
            checks_failed: 0,
            checks_pending: 0,
            changes_requested: false,
            checks_summary: None,
        }
    }

    fn thread() -> PrThread {
        let mut state = AutomationState::default();
        subscribe_prs(&mut state, "job-1", "/work/repo", &[pr(42)], 0);
        state.threads.remove(&pr(42).key()).unwrap()
    }

    #[test]
    fn keys_fold_case_so_one_pr_is_one_thread() {
        let mut upper = pr(7);
        upper.owner = "REBONAI".into();
        assert_eq!(upper.key(), pr(7).key());
        assert_eq!(pr(7).key(), "pr:rebonai/rebon-code#7");
    }

    #[test]
    fn the_first_session_to_open_a_pr_keeps_it() {
        let mut state = AutomationState::default();
        assert!(subscribe_prs(&mut state, "job-1", "/a", &[pr(1), pr(2)], 0));
        assert!(
            !subscribe_prs(&mut state, "job-2", "/b", &[pr(1)], 5),
            "already owned"
        );
        assert_eq!(state.threads[&pr(1).key()].job_id, "job-1");
        assert_eq!(state.threads.len(), 2);
    }

    #[test]
    fn a_failure_is_reported_once_it_settles_and_once_per_failure() {
        let mut t = thread();
        assert_eq!(
            observe_pr(&mut t, open(0, 3, false), 1),
            None,
            "still running"
        );
        assert_eq!(
            observe_pr(&mut t, open(1, 2, false), 2),
            None,
            "not settled yet"
        );
        assert_eq!(
            observe_pr(&mut t, open(1, 0, false), 3),
            Some(PrEvent::ChecksFailed {
                summary: Some("1 check(s) failed".into())
            })
        );
        assert_eq!(observe_pr(&mut t, open(1, 0, false), 4), None, "seen again");
        assert_eq!(observe_pr(&mut t, open(0, 0, false), 5), None, "fixed");
        assert!(
            matches!(
                observe_pr(&mut t, open(2, 0, false), 6),
                Some(PrEvent::ChecksFailed { .. })
            ),
            "a new failure after a fix is news"
        );
        assert_eq!(t.checked_at_ms, Some(6));
    }

    #[test]
    fn a_settled_failure_on_the_first_look_is_news() {
        let mut t = thread();
        assert!(matches!(
            observe_pr(&mut t, open(1, 0, false), 1),
            Some(PrEvent::ChecksFailed { .. })
        ));
    }

    #[test]
    fn a_request_for_changes_is_reported_when_it_appears() {
        let mut t = thread();
        observe_pr(&mut t, open(0, 0, false), 1);
        assert_eq!(
            observe_pr(&mut t, open(0, 0, true), 2),
            Some(PrEvent::ChangesRequested)
        );
        assert_eq!(observe_pr(&mut t, open(0, 0, true), 3), None);
    }

    #[test]
    fn merging_or_closing_ends_the_thread_and_is_reported_once() {
        for (state, event) in [
            (PrState::Merged, PrEvent::Merged),
            (PrState::Closed, PrEvent::Closed),
        ] {
            let mut t = thread();
            observe_pr(&mut t, open(0, 0, false), 1);
            let seen = observe_pr(&mut t, closed(state), 2).expect("reported");
            assert_eq!(seen, event);
            assert!(seen.ends_thread());
            assert_eq!(observe_pr(&mut t, closed(state), 3), None);
        }
        assert!(!PrEvent::ChangesRequested.ends_thread());
    }

    #[test]
    fn a_thread_delivers_at_most_three_events_an_hour_but_always_its_last() {
        let mut t = thread();
        let failed = PrEvent::ChecksFailed { summary: None };
        for at in 0..3 {
            assert!(take_delivery(&mut t, &failed, at));
        }
        assert!(!take_delivery(&mut t, &failed, 10), "spinning is capped");
        assert!(
            take_delivery(&mut t, &PrEvent::Merged, 11),
            "the end gets through"
        );
        assert!(take_delivery(&mut t, &failed, HOUR_MS + 5), "a new hour");
    }

    #[test]
    fn the_longest_unchecked_thread_is_next_and_recent_ones_wait() {
        let mut state = AutomationState::default();
        subscribe_prs(&mut state, "job-1", "/a", &[pr(1), pr(2)], 0);
        assert!(next_thread_to_check(&state, 0).is_some(), "never checked");
        state.threads.get_mut(&pr(1).key()).unwrap().checked_at_ms = Some(100);
        state.threads.get_mut(&pr(2).key()).unwrap().checked_at_ms = Some(50);
        assert_eq!(
            next_thread_to_check(&state, 1_000),
            None,
            "both checked recently"
        );
        assert_eq!(
            next_thread_to_check(&state, 50 + PR_CHECK_INTERVAL_MS + 100),
            Some(pr(2).key())
        );
    }

    #[test]
    fn stale_threads_expire() {
        let mut state = AutomationState::default();
        subscribe_prs(&mut state, "job-1", "/a", &[pr(1)], 0);
        assert!(!expire_threads(&mut state, THREAD_TTL_MS - 1));
        assert!(expire_threads(&mut state, THREAD_TTL_MS));
        assert!(state.threads.is_empty());
    }

    #[test]
    fn prompts_name_the_thread_and_say_what_to_do() {
        let t = thread();
        let failed = event_prompt(
            &t,
            &PrEvent::ChecksFailed {
                summary: Some("2 check(s) failed".into()),
            },
        );
        assert!(
            failed.starts_with("[Thread pr:rebonai/rebon-code#42] "),
            "{failed}"
        );
        assert!(failed.contains("(2 check(s) failed)") && failed.contains("gh pr checks 42"));
        assert!(event_prompt(&t, &PrEvent::ChangesRequested).contains("gh pr view 42 --comments"));
        assert!(event_prompt(&t, &PrEvent::Merged).contains("Pull request RebonAI/Rebon-Code#42"));
        assert!(event_prompt(&t, &PrEvent::Closed).contains("closed without merging"));
    }
}
