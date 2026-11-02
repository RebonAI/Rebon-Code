//! The supervisor's half of the watchdog: cheap checks on the router and its
//! workers, and the alerts that wake the watchdog session.
//!
//! What goes wrong with an agent left to run is seldom a dead process — crash
//! recovery handles those (`crate::autonomy`). It is a live one that gets
//! nowhere. Once a minute, for each job of the running router, the
//! supervisor looks for:
//!
//! - **no progress**: running for a long while without a single new event;
//! - **repeating**: the same tool call, with the same input, several times in
//!   a row at the end of its transcript;
//! - **overspending**: more tokens today than a task should take;
//! - **the router itself**: one turn that runs far too long, or a failure.
//!
//! Each finding is an alert with its evidence and ids, sent once an hour at
//! most per job and kind, batched into one scheduled turn for the watchdog
//! session, which decides what to do (`rebon_core::watchdog_mode`). With no
//! watchdog running, the router hears it instead. No model is asked anything
//! until a check finds something: this is the fast path, the watchdog the
//! slow one.

use std::cell::RefCell;
use std::collections::BTreeMap;

use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundStore, STALE_PID_EXIT_ERROR,
};

const CHECK_INTERVAL_MS: u64 = 60_000;
/// Running this long without a new event is no progress.
const STALL_MS: u64 = 15 * 60 * 1000;
/// The router keeps its turns short; one this long has gone wrong.
const ROUTER_TURN_MS: u64 = 15 * 60 * 1000;
/// This many identical tool calls in a row is repeating.
const REPEATS: usize = 4;
/// Tokens one job may use in a day before it is overspending.
const OVERSPEND_TOKENS: u64 = 2_000_000;
/// One alert per job and kind in this long.
const ALERT_COOLDOWN_MS: u64 = 60 * 60 * 1000;
/// How much of a transcript's end is read for repeated calls.
const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const MAX_EVIDENCE_CHARS: usize = 240;

#[derive(Default)]
struct WatchState {
    last_check_ms: Option<u64>,
    /// Each running job's event count and when it last changed.
    progress: BTreeMap<String, (u64, u64)>,
    /// When the router's current turn was first seen running.
    router_running_since: Option<(String, u64)>,
    /// When each job and kind was last alerted.
    alerted: BTreeMap<(String, &'static str), u64>,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alert {
    pub job_id: String,
    pub kind: &'static str,
    pub evidence: String,
}

/// The supervisor's watch between ticks.
pub(crate) struct Watchdog {
    state: RefCell<WatchState>,
}

/// What the checks read beyond the job records: a job's recent tool calls.
pub(crate) type RecentCalls<'a> = dyn Fn(&BackgroundJobState) -> Vec<String> + 'a;

impl Watchdog {
    pub(crate) fn new() -> Self {
        Self {
            state: RefCell::new(WatchState::default()),
        }
    }

    /// One tick: check once a minute and deliver what was found.
    pub(crate) fn tick(
        &self,
        store: &BackgroundStore,
        jobs: &[BackgroundJobState],
        now_ms: u64,
        task_of: &crate::router_digest::TaskOf<'_>,
    ) {
        let Some(router) = crate::router::current_router(store) else {
            return;
        };
        let recent_calls = |job: &BackgroundJobState| recent_tool_calls(job);
        let alerts = self.check(&router, jobs, now_ms, &recent_calls);
        if alerts.is_empty() {
            return;
        }
        let target = crate::router::current(store, crate::router::WATCHDOG)
            .map(|watchdog| watchdog.identity.job_id)
            .unwrap_or_else(|| router.identity.job_id.clone());
        let id = format!("u-internal-scheduled-watchdog-{now_ms}");
        let text = rebon_tool::cron::mark_scheduled_prompt(&render(&alerts, task_of));
        if let Err(error) =
            rebon_session_host::append_background_scheduled_prompt(store, &target, id, text)
        {
            tracing::warn!(error = %format!("{error:#}"), "rebon: could not deliver watchdog alerts");
        }
    }

    /// The checks, at most once a minute. Alerts already sent within the
    /// cooldown are left out.
    pub(crate) fn check(
        &self,
        router: &BackgroundJobState,
        jobs: &[BackgroundJobState],
        now_ms: u64,
        recent_calls: &RecentCalls<'_>,
    ) -> Vec<Alert> {
        let mut state = self.state.borrow_mut();
        if state
            .last_check_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < CHECK_INTERVAL_MS)
        {
            return Vec::new();
        }
        state.last_check_ms = Some(now_ms);
        let router_id = router.identity.job_id.as_str();
        let today = rebon_session_host::local_day_number(now_ms);
        let mut found = Vec::new();

        // The router itself.
        let router_now = jobs
            .iter()
            .find(|job| job.identity.job_id == router_id)
            .unwrap_or(router);
        match router_now.process.status {
            BackgroundJobStatus::Running => {
                let key = (router_id.to_string(), router_now.process.turn_generation);
                let since = match &state.router_running_since {
                    Some((turn, since)) if *turn == format!("{}:{}", key.0, key.1) => *since,
                    _ => {
                        state.router_running_since = Some((format!("{}:{}", key.0, key.1), now_ms));
                        now_ms
                    }
                };
                if now_ms.saturating_sub(since) >= ROUTER_TURN_MS {
                    found.push(Alert {
                        job_id: router_id.to_string(),
                        kind: "router",
                        evidence: format!(
                            "the router has been on one turn for {} minutes; its turns should take seconds",
                            now_ms.saturating_sub(since) / 60_000
                        ),
                    });
                }
            }
            BackgroundJobStatus::Failed => {
                state.router_running_since = None;
                let error = router_now
                    .outcome
                    .error
                    .as_deref()
                    .unwrap_or("no error recorded");
                // A crash is crash recovery's to bring back; anything else is not.
                if error != STALE_PID_EXIT_ERROR {
                    found.push(Alert {
                        job_id: router_id.to_string(),
                        kind: "router",
                        evidence: format!("the router's last turn failed: {}", clip(error)),
                    });
                }
            }
            _ => state.router_running_since = None,
        }

        // Its workers, and the checks on their tasks.
        let workers: Vec<&BackgroundJobState> = jobs
            .iter()
            .filter(|job| job.identity.parent_job_id.as_deref() == Some(router_id))
            .collect();
        for job in &workers {
            let job_id = job.identity.job_id.clone();
            if job.process.status == BackgroundJobStatus::Running {
                let count = job.outcome.event_count;
                let since = match state.progress.get(&job_id) {
                    Some((seen, since)) if *seen == count => *since,
                    _ => {
                        state.progress.insert(job_id.clone(), (count, now_ms));
                        now_ms
                    }
                };
                if now_ms.saturating_sub(since) >= STALL_MS {
                    found.push(Alert {
                        job_id: job_id.clone(),
                        kind: "no progress",
                        evidence: format!(
                            "running with no new activity for {} minutes",
                            now_ms.saturating_sub(since) / 60_000
                        ),
                    });
                }
                if let Some(repeated) = repeated_call(&recent_calls(job)) {
                    found.push(Alert {
                        job_id: job_id.clone(),
                        kind: "repeating",
                        evidence: repeated,
                    });
                }
            } else {
                state.progress.remove(&job_id);
            }
            let spent = job.outcome.tokens_on_day(today);
            if spent >= OVERSPEND_TOKENS {
                found.push(Alert {
                    job_id,
                    kind: "overspending",
                    evidence: format!("{spent} tokens today (input, output and cache writes)"),
                });
            }
        }
        state
            .progress
            .retain(|job_id, _| workers.iter().any(|job| job.identity.job_id == *job_id));

        found.retain(|alert| {
            let key = (alert.job_id.clone(), alert.kind);
            let fresh = state
                .alerted
                .get(&key)
                .is_none_or(|at| now_ms.saturating_sub(*at) >= ALERT_COOLDOWN_MS);
            if fresh {
                state.alerted.insert(key, now_ms);
            }
            fresh
        });
        state
            .alerted
            .retain(|_, at| now_ms.saturating_sub(*at) < ALERT_COOLDOWN_MS);
        found
    }
}

/// The repeated call at the end of `calls`, when the last few are one and
/// the same.
fn repeated_call(calls: &[String]) -> Option<String> {
    let last = calls.last()?;
    let run = calls.iter().rev().take_while(|call| *call == last).count();
    (run >= REPEATS).then(|| format!("made the same call {run} times in a row: {}", clip(last)))
}

/// A job's last tool calls, oldest first, each as `Name {input}`, read from
/// the end of its transcript.
fn recent_tool_calls(job: &BackgroundJobState) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) =
        rebon_session_host::background_job_transcript_path_in(&rebon_harness::projects_root(), job)
    else {
        return Vec::new();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    tool_calls_in(&text)
}

/// The tool calls in transcript lines, oldest first. The first line may be
/// cut off by the tail and is skipped when it does not parse.
fn tool_calls_in(text: &str) -> Vec<String> {
    let mut calls = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(|kind| kind.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = entry
            .pointer("/message/content")
            .and_then(|content| content.as_array())
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|kind| kind.as_str()) == Some("tool_use") {
                let name = block
                    .get("name")
                    .and_then(|name| name.as_str())
                    .unwrap_or("?");
                let input = block
                    .get("input")
                    .map(|input| input.to_string())
                    .unwrap_or_default();
                calls.push(format!("{name} {input}"));
            }
        }
    }
    calls
}

fn clip(text: &str) -> String {
    let mut clipped: String = text.chars().take(MAX_EVIDENCE_CHARS).collect();
    if text.chars().count() > MAX_EVIDENCE_CHARS {
        clipped.push('…');
    }
    clipped
}

fn render(alerts: &[Alert], task_of: &crate::router_digest::TaskOf<'_>) -> String {
    let lines: Vec<String> = alerts
        .iter()
        .map(|alert| {
            let task = task_of(&alert.job_id)
                .map(|task| format!(" (task {task})"))
                .unwrap_or_default();
            format!(
                "- {}{task} {}: {}",
                alert.job_id, alert.kind, alert.evidence
            )
        })
        .collect();
    format!(
        "[Watchdog alerts]\n{}\n\nLook before you act: Recall the ids. Decide for each what, if anything, to do.",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::BackgroundRuntimeFields;

    fn job(id: &str, parent: Option<&str>, status: BackgroundJobStatus) -> BackgroundJobState {
        let mut job = BackgroundJobState::new(
            "p".into(),
            ".".into(),
            BackgroundRuntimeFields {
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
            None,
        );
        job.identity.job_id = id.into();
        job.identity.parent_job_id = parent.map(str::to_string);
        job.process.status = status;
        job
    }

    const MIN: u64 = 60_000;

    #[test]
    fn a_worker_with_no_new_activity_is_flagged_once_an_hour() {
        let watchdog = Watchdog::new();
        let router = job("r", None, BackgroundJobStatus::Idle);
        let worker = job("w", Some("r"), BackgroundJobStatus::Running);
        let jobs = [router.clone(), worker];
        let none = |_: &BackgroundJobState| Vec::new();
        assert!(watchdog.check(&router, &jobs, 0, &none).is_empty());
        assert!(watchdog.check(&router, &jobs, 14 * MIN, &none).is_empty());
        let alerts = watchdog.check(&router, &jobs, 16 * MIN, &none);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, "no progress");
        assert!(
            watchdog.check(&router, &jobs, 30 * MIN, &none).is_empty(),
            "cooldown"
        );
        assert_eq!(watchdog.check(&router, &jobs, 77 * MIN, &none).len(), 1);
    }

    #[test]
    fn activity_resets_the_clock_and_checks_wait_a_minute() {
        let watchdog = Watchdog::new();
        let router = job("r", None, BackgroundJobStatus::Idle);
        let mut worker = job("w", Some("r"), BackgroundJobStatus::Running);
        let none = |_: &BackgroundJobState| Vec::new();
        watchdog.check(&router, &[router.clone(), worker.clone()], 0, &none);
        worker.outcome.event_count = 5;
        watchdog.check(&router, &[router.clone(), worker.clone()], 10 * MIN, &none);
        assert!(watchdog
            .check(&router, &[router.clone(), worker.clone()], 20 * MIN, &none)
            .is_empty());
        assert!(
            watchdog
                .check(&router, &[router.clone(), worker], 20 * MIN + 1, &none)
                .is_empty(),
            "within the minute"
        );
    }

    #[test]
    fn the_same_call_again_and_again_is_repeating() {
        let watchdog = Watchdog::new();
        let router = job("r", None, BackgroundJobStatus::Idle);
        let worker = job("w", Some("r"), BackgroundJobStatus::Running);
        let calls = |_: &BackgroundJobState| {
            let mut calls = vec!["Read {\"path\":\"a\"}".to_string()];
            calls.extend(std::iter::repeat_n(
                "Bash {\"command\":\"cargo test\"}".to_string(),
                5,
            ));
            calls
        };
        let alerts = watchdog.check(&router, &[router.clone(), worker], 0, &calls);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, "repeating");
        assert!(alerts[0].evidence.contains("5 times in a row: Bash"));
    }

    #[test]
    fn a_router_on_one_turn_too_long_or_failed_is_flagged_but_not_a_crash() {
        let watchdog = Watchdog::new();
        let mut router = job("r", None, BackgroundJobStatus::Running);
        let none = |_: &BackgroundJobState| Vec::new();
        watchdog.check(&router, std::slice::from_ref(&router), 0, &none);
        let alerts = watchdog.check(&router, std::slice::from_ref(&router), 16 * MIN, &none);
        assert_eq!(alerts[0].kind, "router");

        let watchdog = Watchdog::new();
        router.process.status = BackgroundJobStatus::Failed;
        router.outcome.error = Some(STALE_PID_EXIT_ERROR.into());
        assert!(watchdog
            .check(&router, std::slice::from_ref(&router), 0, &none)
            .is_empty());
        router.outcome.error = Some("quota exceeded".into());
        let alerts = watchdog.check(&router, std::slice::from_ref(&router), 2 * MIN, &none);
        assert!(alerts[0].evidence.contains("quota exceeded"));
    }

    #[test]
    fn a_job_spending_too_much_today_is_overspending() {
        let watchdog = Watchdog::new();
        let router = job("r", None, BackgroundJobStatus::Idle);
        let mut worker = job("w", Some("r"), BackgroundJobStatus::Idle);
        let now = 1_700_000_000_000;
        worker.outcome.record_turn_usage(
            &rebon_types::Usage {
                input_tokens: 2_100_000,
                ..Default::default()
            },
            rebon_session_host::local_day_number(now),
        );
        let none = |_: &BackgroundJobState| Vec::new();
        let alerts = watchdog.check(&router, &[router.clone(), worker], now, &none);
        assert_eq!(alerts[0].kind, "overspending");
    }

    #[test]
    fn tool_calls_are_read_from_transcript_lines() {
        let text = concat!(
            "cut off line\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"},",
            "{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[]}}\n"
        );
        assert_eq!(tool_calls_in(text), ["Bash {\"command\":\"ls\"}"]);
    }

    #[test]
    fn alerts_name_their_tasks() {
        let text = render(
            &[Alert {
                job_id: "w".into(),
                kind: "repeating",
                evidence: "x".into(),
            }],
            &|_| Some("t-1".into()),
        );
        assert!(text.starts_with("[Watchdog alerts]\n- w (task t-1) repeating: x"));
    }
}
