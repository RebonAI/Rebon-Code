//! Worker digests: how the router hears what its workers did.
//!
//! Each tick the supervisor looks at the router's workers. A worker that
//! finished a turn, failed, is waiting for an answer, or was stopped gets a
//! line. Lines from the same few seconds are batched into one scheduled turn
//! for the router, so a fan-out of workers finishing together interrupts it
//! once rather than once per worker. Each line carries the worker's id; the
//! full reply stays in the worker's transcript, where `Recall` finds it.
//!
//! What has been reported lives in `<store>/router-digest.json`, which only
//! the supervisor writes, so a restart neither repeats nor loses a line.
//!
//! The same tick publishes `<store>/router-roster.json`: the router's session
//! and its workers' sessions, for the desktop app, which pins the router and
//! folds its workers under it. It is removed while no router runs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use rebon_session_host::{BackgroundJobState, BackgroundJobStatus, BackgroundStore};
use serde::{Deserialize, Serialize};

const STATE_FILE_NAME: &str = "router-digest.json";
/// How long the first line waits for others before the digest goes out.
const BATCH_MS: u64 = 15_000;
const MAX_LINE_CHARS: usize = 300;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DigestState {
    #[serde(default)]
    router_job_id: Option<String>,
    /// Each worker's state as last seen: its status and turn.
    #[serde(default)]
    seen: BTreeMap<String, (String, u64)>,
    /// Lines waiting to go out.
    #[serde(default)]
    pending: Vec<String>,
    #[serde(default)]
    pending_since_ms: Option<u64>,
}

/// Who the router and its workers are, for the desktop app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Roster {
    router_job_id: String,
    router_session_id: Option<String>,
    /// The watchdog beside the router, when one runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watchdog_session_id: Option<String>,
    workers: Vec<RosterWorker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RosterWorker {
    job_id: String,
    session_id: Option<String>,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_active_responsibility: Option<bool>,
}

pub(crate) fn worker_has_responsibility(
    job: &BackgroundJobState,
    work: Option<bool>,
    threads: Option<bool>,
) -> Option<bool> {
    if matches!(
        job.process.status,
        BackgroundJobStatus::Queued
            | BackgroundJobStatus::Running
            | BackgroundJobStatus::NeedsInput
    ) || !job.identity.pending_prompts.is_empty()
        || job.outcome.pending_permission.is_some()
        || job.outcome.pull_requests.iter().any(|pr| {
            pr.state.as_deref().is_none_or(|state| {
                !matches!(state.to_ascii_uppercase().as_str(), "MERGED" | "CLOSED")
            })
        })
        || work == Some(true)
        || threads == Some(true)
    {
        Some(true)
    } else {
        work.zip(threads).map(|_| false)
    }
}

type ResponsibilityOf<'a> = dyn Fn(&BackgroundJobState) -> Option<bool> + 'a;

fn roster(
    router: &BackgroundJobState,
    watchdog: Option<&BackgroundJobState>,
    jobs: &[BackgroundJobState],
    responsibility_of: &ResponsibilityOf<'_>,
) -> Roster {
    let router_id = router.identity.job_id.as_str();
    Roster {
        router_job_id: router_id.to_string(),
        router_session_id: router.identity.session_id.clone(),
        watchdog_session_id: watchdog.and_then(|watchdog| watchdog.identity.session_id.clone()),
        workers: jobs
            .iter()
            .filter(|job| job.identity.parent_job_id.as_deref() == Some(router_id))
            .map(|job| RosterWorker {
                job_id: job.identity.job_id.clone(),
                session_id: job.identity.session_id.clone(),
                title: job.identity.name.clone(),
                has_active_responsibility: responsibility_of(job),
            })
            .collect(),
    }
}

/// The supervisor's digest state between ticks.
pub(crate) struct RouterDigest {
    path: PathBuf,
    roster_path: PathBuf,
    state: RefCell<Option<DigestState>>,
    /// The roster as last written; `None` before the first tick.
    published: RefCell<Option<Option<Roster>>>,
}

impl RouterDigest {
    pub(crate) fn new(store: &BackgroundStore) -> Self {
        Self {
            path: store.root().join(STATE_FILE_NAME),
            roster_path: store.root().join(crate::router::ROSTER_FILE_NAME),
            state: RefCell::new(None),
            published: RefCell::new(None),
        }
    }

    /// Write the roster when it changed, and remove it while no router runs.
    fn publish(&self, roster: Option<Roster>) {
        let mut published = self.published.borrow_mut();
        if published.as_ref() == Some(&roster) {
            return;
        }
        let result = match &roster {
            Some(roster) => serde_json::to_vec_pretty(roster)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    rebon_session::write_file_atomically(&self.roster_path, &bytes)
                        .map_err(anyhow::Error::from)
                }),
            None => match std::fs::remove_file(&self.roster_path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
                _ => Ok(()),
            },
        };
        match result {
            Ok(()) => *published = Some(roster),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "rebon: could not publish the router roster");
            }
        }
    }

    /// One tick. `true` while lines wait to go out, which keeps the
    /// supervisor up for them.
    /// `notices` are what the task ledger's pass found; `task_of` names the
    /// task a worker works on.
    pub(crate) fn tick(
        &self,
        store: &BackgroundStore,
        jobs: &[BackgroundJobState],
        now_ms: u64,
        notices: &[rebon_work::TaskNotice],
        task_of: &TaskOf<'_>,
        responsibility_of: &ResponsibilityOf<'_>,
    ) -> bool {
        let Some(router) = crate::router::current_router(store) else {
            self.publish(None);
            return false;
        };
        let watchdog = crate::router::current(store, crate::router::WATCHDOG);
        self.publish(Some(roster(
            &router,
            watchdog.as_ref(),
            jobs,
            responsibility_of,
        )));
        let mut slot = self.state.borrow_mut();
        let state = slot.get_or_insert_with(|| {
            std::fs::read(&self.path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        });
        let before = state.clone();
        let router_id = router.identity.job_id.as_str();
        let last_reply = |job: &BackgroundJobState| {
            rebon_session_host::background_job_transcript_path_in(
                &rebon_harness::projects_root(),
                job,
            )
            .and_then(|transcript| {
                rebon_session::last_turn_assistant_text(&transcript)
                    .ok()
                    .flatten()
            })
        };
        collect(state, router_id, jobs, now_ms, &last_reply, task_of);
        for notice in notices
            .iter()
            .filter(|notice| notice.router_job_id.as_deref() == Some(router_id))
        {
            state.pending.push(format!(
                "- task {} \"{}\" {}",
                notice.task_id,
                notice.title,
                clip(&notice.text)
            ));
            state.pending_since_ms.get_or_insert(now_ms);
        }
        let due = state
            .pending_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) >= BATCH_MS);
        if due {
            let id = format!("u-internal-scheduled-digest-{now_ms}");
            let text = rebon_tool::cron::mark_scheduled_prompt(&render(&state.pending));
            match rebon_session_host::append_background_scheduled_prompt(store, router_id, id, text)
            {
                Ok(_) => {
                    state.pending.clear();
                    state.pending_since_ms = None;
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "rebon: could not deliver a worker digest; retrying");
                }
            }
        }
        if *state != before {
            let written = serde_json::to_vec_pretty(state)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| {
                    rebon_session::write_file_atomically(&self.path, &bytes)
                        .map_err(anyhow::Error::from)
                });
            if let Err(error) = written {
                tracing::warn!(error = %format!("{error:#}"), "rebon: could not save the worker digest");
            }
        }
        !state.pending.is_empty()
    }
}

/// A worker's last reply, read only for a worker that just finished a turn.
type LastReply<'a> = dyn Fn(&BackgroundJobState) -> Option<String> + 'a;
/// The task a worker works on.
pub(crate) type TaskOf<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Note every worker of `router_id` whose state moved to one worth a line.
fn collect(
    state: &mut DigestState,
    router_id: &str,
    jobs: &[BackgroundJobState],
    now_ms: u64,
    last_reply: &LastReply<'_>,
    task_of: &TaskOf<'_>,
) {
    if state.router_job_id.as_deref() != Some(router_id) {
        *state = DigestState {
            router_job_id: Some(router_id.to_string()),
            ..DigestState::default()
        };
    }
    let workers: Vec<&BackgroundJobState> = jobs
        .iter()
        .filter(|job| job.identity.parent_job_id.as_deref() == Some(router_id))
        .collect();
    for job in &workers {
        let key = match job
            .outcome
            .pending_permission
            .as_ref()
            .filter(|_| job.process.status == BackgroundJobStatus::NeedsInput)
        {
            Some(query) => format!("needs_input:{}", query.query_id),
            None => status_key(job.process.status).to_string(),
        };
        let now = (key, job.process.turn_generation);
        let changed = state.seen.get(&job.identity.job_id) != Some(&now);
        // A worker or check on a task reports through the task: done once it
        // passed its check, stuck, blocked. Its turns ending say nothing yet,
        // and reporting them would have the router answer before the check
        // has. Only a question it waits on comes straight through.
        let tasked = task_of(&job.identity.job_id).is_some();
        let speaks = !tasked || job.process.status == BackgroundJobStatus::NeedsInput;
        if changed {
            if let Some(line) = speaks.then(|| line(job, last_reply, task_of)).flatten() {
                state.pending.push(line);
                state.pending_since_ms.get_or_insert(now_ms);
            }
            state.seen.insert(job.identity.job_id.clone(), now);
        }
    }
    state
        .seen
        .retain(|id, _| workers.iter().any(|job| job.identity.job_id == *id));
}

fn status_key(status: BackgroundJobStatus) -> &'static str {
    match status {
        BackgroundJobStatus::Queued => "queued",
        BackgroundJobStatus::Running => "running",
        BackgroundJobStatus::NeedsInput => "needs_input",
        BackgroundJobStatus::Idle => "idle",
        BackgroundJobStatus::Succeeded => "succeeded",
        BackgroundJobStatus::Failed => "failed",
        BackgroundJobStatus::Stopped => "stopped",
    }
}

/// The line for a worker's new state, if the state is worth one.
fn line(
    job: &BackgroundJobState,
    last_reply: &LastReply<'_>,
    task_of: &TaskOf<'_>,
) -> Option<String> {
    let id = &job.identity.job_id;
    let name = &job.identity.name;
    let what = match job.process.status {
        BackgroundJobStatus::Queued | BackgroundJobStatus::Running => return None,
        // What the worker said leads: its summary is often only a status
        // line, which would send the router to Recall for every worker.
        BackgroundJobStatus::Idle | BackgroundJobStatus::Succeeded => {
            let reply = last_reply(job);
            let first_line = |text: &str| {
                text.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(str::to_string)
            };
            let said = reply
                .as_deref()
                .and_then(first_line)
                .or_else(|| job.outcome.summary.as_deref().and_then(first_line))
                .unwrap_or_else(|| "no reply yet".to_string());
            format!("finished a turn: {}", clip(&said))
        }
        BackgroundJobStatus::Failed => format!(
            "failed: {}",
            clip(job.outcome.error.as_deref().unwrap_or("no error recorded"))
        ),
        BackgroundJobStatus::NeedsInput => {
            if let Some(query) = job.outcome.pending_permission.as_ref().filter(|query| {
                query.tool.as_deref() == Some("AskUserQuestion")
                    && job.identity.runtime.permission_mode.as_deref() == Some("auto")
            }) {
                let question = query
                    .tool_input
                    .as_ref()
                    .and_then(|input| input.get("questions"))
                    .and_then(serde_json::Value::as_array)
                    .and_then(|questions| questions.first())
                    .and_then(|question| question.get("question"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("question details available through Recall");
                return Some(format!("- {id} \"{name}\" is waiting on questionId {}: {}. Recall the worker, decide from existing authorization, then ContinueWorker with questionId; do not queue behind the question.", query.query_id, clip(question)));
            }
            let asking = job.outcome.pending_permission.as_ref().and_then(|query| {
                query
                    .title
                    .as_deref()
                    .or(query.tool.as_deref())
                    .map(str::to_string)
            });
            match asking {
                Some(asking) => format!("is waiting for an answer: {}", clip(&asking)),
                None => "is waiting for an answer".to_string(),
            }
        }
        BackgroundJobStatus::Stopped => "was stopped".to_string(),
    };
    let task = task_of(id)
        .map(|task| format!(" (task {task})"))
        .unwrap_or_default();
    Some(format!("- {id} \"{name}\"{task} {what}"))
}

fn clip(text: &str) -> String {
    let mut clipped: String = text.chars().take(MAX_LINE_CHARS).collect();
    if text.chars().count() > MAX_LINE_CHARS {
        clipped.push('…');
    }
    clipped
}

fn render(lines: &[String]) -> String {
    format!(
        "[Worker digest]\n{}\n\nRecall an id to see its last reply. Decide what each one needs; most need nothing.",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::BackgroundRuntimeFields;

    fn worker(
        id: &str,
        parent: &str,
        status: BackgroundJobStatus,
        turn: u64,
    ) -> BackgroundJobState {
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
            Some(format!("task {id}")),
        );
        job.identity.job_id = id.into();
        job.identity.parent_job_id = Some(parent.into());
        job.process.status = status;
        job.process.turn_generation = turn;
        job
    }

    #[test]
    fn a_worker_is_reported_once_per_settled_turn() {
        let mut state = DigestState::default();
        let mut job = worker("w1", "r", BackgroundJobStatus::Running, 1);
        collect(
            &mut state,
            "r",
            std::slice::from_ref(&job),
            0,
            &|_| None,
            &|_| None,
        );
        assert!(state.pending.is_empty(), "running is not news");

        job.process.status = BackgroundJobStatus::Idle;
        job.outcome.summary = Some("\nOpened a pull request\nmore".into());
        collect(
            &mut state,
            "r",
            std::slice::from_ref(&job),
            10,
            &|_| None,
            &|_| None,
        );
        collect(
            &mut state,
            "r",
            std::slice::from_ref(&job),
            20,
            &|_| None,
            &|_| None,
        );
        assert_eq!(
            state.pending,
            ["- w1 \"task w1\" finished a turn: Opened a pull request"]
        );
        assert_eq!(state.pending_since_ms, Some(10));

        job.process.turn_generation = 2;
        job.process.status = BackgroundJobStatus::Failed;
        job.outcome.error = Some("quota".into());
        collect(
            &mut state,
            "r",
            std::slice::from_ref(&job),
            30,
            &|_| None,
            &|_| None,
        );
        assert_eq!(state.pending.len(), 2);
        assert!(state.pending[1].ends_with("failed: quota"));
    }

    #[test]
    fn a_finished_turn_is_told_by_the_workers_own_words() {
        let mut state = DigestState::default();
        let mut job = worker("w1", "r", BackgroundJobStatus::Idle, 1);
        job.outcome.summary = Some("responding: done".into());
        let reply =
            |_: &BackgroundJobState| Some("\n  Fixed add(); 6 tests pass.\nDetails…".to_string());
        collect(
            &mut state,
            "r",
            std::slice::from_ref(&job),
            0,
            &reply,
            &|_| None,
        );
        assert_eq!(
            state.pending,
            ["- w1 \"task w1\" finished a turn: Fixed add(); 6 tests pass."]
        );
    }

    #[test]
    fn a_worker_on_a_task_speaks_only_when_it_waits_for_an_answer() {
        let mut state = DigestState::default();
        let mut job = worker("w1", "r", BackgroundJobStatus::Idle, 1);
        let on_task = |_: &str| Some("t-9".to_string());
        collect(
            &mut state,
            "r",
            std::slice::from_ref(&job),
            0,
            &|_| None,
            &on_task,
        );
        assert!(
            state.pending.is_empty(),
            "the task reports once it is checked"
        );
        job.process.status = BackgroundJobStatus::NeedsInput;
        job.process.turn_generation = 2;
        collect(
            &mut state,
            "r",
            std::slice::from_ref(&job),
            5,
            &|_| None,
            &on_task,
        );
        assert_eq!(
            state.pending,
            ["- w1 \"task w1\" (task t-9) is waiting for an answer"]
        );
    }

    #[test]
    fn each_question_in_one_turn_reaches_the_router_even_after_digest_reload() {
        let mut state = DigestState::default();
        let mut job = worker("w1", "r", BackgroundJobStatus::NeedsInput, 1);
        job.identity.runtime.permission_mode = Some("auto".into());
        for id in [42, 43] {
            job.outcome.pending_permission = Some(
                serde_json::from_value(serde_json::json!({
                    "queryId": id, "turnGeneration": 1, "tool": "AskUserQuestion",
                    "title": "Answer questions?", "options": [],
                    "toolInput": {"questions": [{"question": "Choose a patch strategy"}]}
                }))
                .unwrap(),
            );
            collect(
                &mut state,
                "r",
                std::slice::from_ref(&job),
                id,
                &|_| None,
                &|_| Some("t-9".into()),
            );
            assert!(state
                .pending
                .last()
                .unwrap()
                .contains(&format!("questionId {id}")));
            assert!(state
                .pending
                .last()
                .unwrap()
                .contains("Choose a patch strategy"));
            state = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        }
        assert_eq!(state.pending.len(), 2);
        collect(&mut state, "r", &[job], 44, &|_| None, &|_| None);
        assert_eq!(
            state.pending.len(),
            2,
            "same question is not reported again"
        );
    }

    #[test]
    fn only_the_routers_own_workers_count_and_a_new_router_starts_over() {
        let mut state = DigestState::default();
        let mine = worker("w1", "r", BackgroundJobStatus::Idle, 1);
        let other = worker("w2", "someone", BackgroundJobStatus::Idle, 1);
        collect(
            &mut state,
            "r",
            &[mine.clone(), other],
            0,
            &|_| None,
            &|_| None,
        );
        assert_eq!(state.pending.len(), 1);
        collect(&mut state, "r2", &[mine], 5, &|_| None, &|_| None);
        assert!(state.pending.is_empty(), "w1 is not r2's");
        assert_eq!(state.router_job_id.as_deref(), Some("r2"));
    }

    #[test]
    fn the_roster_lists_the_routers_session_and_its_workers() {
        let mut router = worker("r", "nobody", BackgroundJobStatus::Idle, 1);
        router.identity.parent_job_id = None;
        router.identity.session_id = Some("router-session".into());
        let mut mine = worker("w1", "r", BackgroundJobStatus::Running, 1);
        mine.identity.session_id = Some("s1".into());
        let other = worker("w2", "someone", BackgroundJobStatus::Idle, 1);
        let roster = roster(&router, None, &[router.clone(), mine, other], &|job| {
            worker_has_responsibility(job, Some(false), Some(false))
        });
        assert_eq!(roster.router_session_id.as_deref(), Some("router-session"));
        assert_eq!(roster.workers.len(), 1);
        assert_eq!(roster.workers[0].session_id.as_deref(), Some("s1"));
        assert_eq!(roster.workers[0].title, "task w1");
        assert_eq!(roster.workers[0].has_active_responsibility, Some(true));
        let wire = serde_json::to_value(&roster).unwrap();
        assert_eq!(wire["workers"][0]["hasActiveResponsibility"], true);
    }

    #[test]
    fn only_known_unowned_terminal_workers_have_no_responsibility() {
        for status in [
            BackgroundJobStatus::Idle,
            BackgroundJobStatus::Succeeded,
            BackgroundJobStatus::Failed,
            BackgroundJobStatus::Stopped,
        ] {
            let job = worker("w1", "r", status, 1);
            assert_eq!(
                worker_has_responsibility(&job, Some(false), Some(false)),
                Some(false)
            );
            assert_eq!(worker_has_responsibility(&job, None, Some(false)), None);
            assert_eq!(worker_has_responsibility(&job, Some(false), None), None);
            assert_eq!(
                worker_has_responsibility(&job, Some(true), None),
                Some(true)
            );
            assert_eq!(
                worker_has_responsibility(&job, None, Some(true)),
                Some(true)
            );
        }
    }

    #[test]
    fn active_queued_and_waiting_workers_cannot_be_archived() {
        for status in [
            BackgroundJobStatus::Running,
            BackgroundJobStatus::Queued,
            BackgroundJobStatus::NeedsInput,
        ] {
            let job = worker("w1", "r", status, 1);
            assert_eq!(
                worker_has_responsibility(&job, Some(false), Some(false)),
                Some(true)
            );
        }
        let mut job = worker("w1", "r", BackgroundJobStatus::Succeeded, 1);
        job.identity.pending_prompts.push(
            rebon_session_host::PendingPrompt::new(
                "pending".into(),
                "next task".into(),
                Vec::new(),
                1,
            )
            .unwrap(),
        );
        assert_eq!(
            worker_has_responsibility(&job, Some(false), Some(false)),
            Some(true)
        );
    }

    #[test]
    fn open_or_unknown_prs_protect_workers_but_closed_prs_do_not() {
        let mut job = worker("w1", "r", BackgroundJobStatus::Succeeded, 1);
        let pr: rebon_session_host::BackgroundPullRequestStatus = serde_json::from_value(serde_json::json!({
            "url": "https://github.com/RebonAI/Rebon-Code/pull/42", "owner": "RebonAI", "repo": "Rebon-Code", "number": 42,
            "state": "OPEN", "updatedAtMs": 1
        })).unwrap();
        job.outcome.pull_requests.push(pr);
        for state in [Some("OPEN"), None, Some("UNKNOWN")] {
            job.outcome.pull_requests[0].state = state.map(str::to_string);
            assert_eq!(
                worker_has_responsibility(&job, Some(false), Some(false)),
                Some(true)
            );
        }
        for state in ["CLOSED", "MERGED", "closed"] {
            job.outcome.pull_requests[0].state = Some(state.into());
            assert_eq!(
                worker_has_responsibility(&job, Some(false), Some(false)),
                Some(false)
            );
        }
    }

    #[test]
    fn a_roster_without_responsibility_stays_unknown_and_round_trips() {
        let old = serde_json::json!({"jobId": "w1", "sessionId": "s1", "title": "past work"});
        let worker: RosterWorker = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(worker.has_active_responsibility, None);
        assert_eq!(serde_json::to_value(worker).unwrap(), old);
        for active in [false, true] {
            let wire = serde_json::json!({"jobId": "w1", "sessionId": "s1", "title": "past work", "hasActiveResponsibility": active});
            let worker: RosterWorker = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(worker.has_active_responsibility, Some(active));
            assert_eq!(serde_json::to_value(worker).unwrap(), wire);
        }
    }

    #[test]
    fn the_digest_names_ids_to_recall() {
        let text = render(&["- w1 \"t\" was stopped".to_string()]);
        assert!(text.starts_with("[Worker digest]\n- w1"));
        assert!(text.contains("Recall an id"));
    }
}
