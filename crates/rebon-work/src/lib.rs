//! The router's task ledger.
//!
//! A task is a unit of done: a goal in a project, what "done" means for it,
//! and the tasks it waits for. The router plans tasks; the background
//! supervisor dispatches each one to a worker once what it waits for is
//! done, within a limit on how many run at once, and follows the worker
//! until the task settles. Nobody has to watch it in between.
//!
//! Two kinds of file under `<home>/work/`, each with one writer:
//!
//! - `ledger.json` ([`Ledger`]): every task and where it stands. Written
//!   only by the supervisor, which runs [`run_pass`] on its tick.
//! - `requests/*.json` ([`WorkRequest`]): plans, bindings and cancellations
//!   from anyone else (the router's tools, later the desktop app), taken
//!   and applied by the supervisor at the start of its pass.
//!
//! Task ids are chosen by whoever files the request ([`new_task_id`]), so a
//! tool can answer with the id at once, before the supervisor has seen it.
//!
//! A task is not done because its worker says so. When the worker finishes,
//! a fixed acceptance check runs: another worker, told only what the task
//! was and what done means, looks at the actual state of the project, runs
//! what shows whether it holds, and ends with a verdict. A pass makes the
//! task done. A fail goes back to the task's worker as its next turn, up to
//! [`MAX_REWORK_ROUNDS`] times, before the task is stuck and the router
//! decides. The router hears about a task once it is done, stuck or
//! blocked; not each time a worker finishes a turn.
//!
//! Launching workers and reading jobs are the caller's ([`Hands`]). This
//! crate names no session host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const WORK_DIR_NAME: &str = "work";
pub const LEDGER_FILE_NAME: &str = "ledger.json";
pub const REQUESTS_DIR_NAME: &str = "requests";
pub const LEDGER_VERSION: u32 = 1;
/// How many events a task keeps.
pub const MAX_TASK_EVENTS: usize = 32;
/// How many times a failed acceptance check sends the work back before the
/// task is stuck.
pub const MAX_REWORK_ROUNDS: u32 = 3;
/// How many times one acceptance check is started when it breaks down or
/// gives no verdict.
pub const MAX_CHECK_ATTEMPTS: u32 = 2;

pub fn work_dir(home: &Path) -> PathBuf {
    home.join(WORK_DIR_NAME)
}

pub fn ledger_path(home: &Path) -> PathBuf {
    work_dir(home).join(LEDGER_FILE_NAME)
}

pub fn requests_dir(home: &Path) -> PathBuf {
    work_dir(home).join(REQUESTS_DIR_NAME)
}

/// Where a task stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting to be dispatched: for the tasks it depends on, or for room.
    Planned,
    /// A worker is on it.
    Running,
    /// Its worker says it finished; the acceptance check is looking.
    Verifying,
    /// It passed its acceptance check.
    Done,
    /// It cannot go on without a decision: its worker failed or is gone, it
    /// kept failing its acceptance check, or it could not be dispatched.
    Stuck,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Verifying => "verifying",
            Self::Done => "done",
            Self::Stuck => "stuck",
            Self::Cancelled => "cancelled",
        }
    }

    /// Nothing more happens to it on its own.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Done | Self::Stuck | Self::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEvent {
    pub at_ms: u64,
    pub what: String,
}

/// One acceptance check's verdict on a task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskVerdict {
    pub at_ms: u64,
    pub passed: bool,
    pub reason: String,
    pub verifier_job_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub title: String,
    /// The project directory the task's worker works in.
    pub project: String,
    /// What the worker is told: everything it needs to know.
    pub goal: String,
    /// What must hold for the task to be done.
    #[serde(default)]
    pub done_when: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub isolate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    /// The router that planned it; its workers are that router's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router_job_id: Option<String>,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_job_id: Option<String>,
    /// How many workers were dispatched for it.
    #[serde(default)]
    pub attempts: u32,
    /// The acceptance check looking at it now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_job_id: Option<String>,
    /// Which start of the current check this is.
    #[serde(default)]
    pub verifier_attempts: u32,
    /// How many times a failed check sent the work back.
    #[serde(default)]
    pub rework_rounds: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<TaskVerdict>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Why it stands where it does, when that needs saying: what it waits
    /// for, why it is stuck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<TaskEvent>,
}

impl Task {
    fn record(&mut self, now_ms: u64, what: impl Into<String>) {
        self.history.push(TaskEvent {
            at_ms: now_ms,
            what: what.into(),
        });
        if self.history.len() > MAX_TASK_EVENTS {
            let excess = self.history.len() - MAX_TASK_EVENTS;
            self.history.drain(..excess);
        }
        self.updated_at_ms = now_ms;
    }

    fn set(&mut self, status: TaskStatus, note: Option<String>, now_ms: u64) {
        let what = match &note {
            Some(note) => format!("{}: {note}", status.as_str()),
            None => status.as_str().to_string(),
        };
        self.status = status;
        self.note = note;
        self.record(now_ms, what);
    }
}

/// Every task, by id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub tasks: BTreeMap<String, Task>,
}

impl Ledger {
    /// The ledger at `path`; a missing file is an empty ledger.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a task ledger", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ledger = Self {
            version: LEDGER_VERSION,
            tasks: self.tasks.clone(),
        };
        rebon_session::write_file_atomically(path, &serde_json::to_vec_pretty(&ledger)?)
            .with_context(|| format!("write {}", path.display()))
    }

    /// The task `job_id` works on or checks, if any.
    pub fn task_of_worker(&self, job_id: &str) -> Option<&Task> {
        self.tasks.values().find(|task| {
            task.worker_job_id.as_deref() == Some(job_id)
                || task.verifier_job_id.as_deref() == Some(job_id)
                || task
                    .verdicts
                    .iter()
                    .any(|verdict| verdict.verifier_job_id == job_id)
        })
    }
}

/// A task as it is planned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTask {
    pub id: String,
    pub title: String,
    pub project: String,
    pub goal: String,
    #[serde(default)]
    pub done_when: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub isolate: bool,
    #[serde(default)]
    pub model_profile: Option<String>,
}

impl NewTask {
    fn into_task(self, router_job_id: Option<String>, now_ms: u64) -> Task {
        Task {
            id: self.id,
            title: self.title,
            project: self.project,
            goal: self.goal,
            done_when: self.done_when,
            depends_on: self.depends_on,
            isolate: self.isolate,
            model_profile: self.model_profile,
            router_job_id,
            status: TaskStatus::Planned,
            worker_job_id: None,
            attempts: 0,
            verifier_job_id: None,
            verifier_attempts: 0,
            rework_rounds: 0,
            verdicts: Vec::new(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            note: None,
            history: vec![TaskEvent {
                at_ms: now_ms,
                what: "planned".into(),
            }],
        }
    }
}

/// A change to the ledger from someone other than the supervisor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkRequest {
    /// New tasks. One whose id is already taken is left out.
    Plan {
        #[serde(default)]
        router_job_id: Option<String>,
        tasks: Vec<NewTask>,
        requested_at_ms: u64,
    },
    /// A task whose worker was already started (`DispatchWorker`).
    Bind {
        #[serde(default)]
        router_job_id: Option<String>,
        task: NewTask,
        worker_job_id: String,
        requested_at_ms: u64,
    },
    /// Stop a task. Its worker is stopped by whoever asks.
    Cancel {
        task_id: String,
        #[serde(default)]
        reason: Option<String>,
        requested_at_ms: u64,
    },
}

impl WorkRequest {
    fn requested_at_ms(&self) -> u64 {
        match self {
            Self::Plan {
                requested_at_ms, ..
            }
            | Self::Bind {
                requested_at_ms, ..
            }
            | Self::Cancel {
                requested_at_ms, ..
            } => *requested_at_ms,
        }
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh task id: unique across the processes that file requests, short
/// enough to read back.
pub fn new_task_id(now_ms: u64) -> String {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(
        "t-{}{:x}{}",
        to_base36(now_ms),
        std::process::id() % 4096,
        to_base36(n)
    )
}

fn to_base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("ascii")
}

/// File a request for the supervisor's next pass.
pub fn file_request(home: &Path, request: &WorkRequest) -> anyhow::Result<()> {
    let dir = requests_dir(home);
    std::fs::create_dir_all(&dir)?;
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{:016}-{}-{n}.json",
        request.requested_at_ms(),
        std::process::id()
    ));
    rebon_session::write_file_atomically(&path, &serde_json::to_vec_pretty(request)?)
        .with_context(|| format!("write {}", path.display()))
}

/// Take every pending request, oldest first, removing each so it applies
/// once. One that cannot be read is dropped.
pub fn take_requests(home: &Path) -> Vec<WorkRequest> {
    let Ok(entries) = std::fs::read_dir(requests_dir(home)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
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

/// Apply one request. `true` when the ledger changed.
pub fn apply_request(ledger: &mut Ledger, request: WorkRequest, now_ms: u64) -> bool {
    match request {
        WorkRequest::Plan {
            router_job_id,
            tasks,
            ..
        } => {
            let mut changed = false;
            for task in tasks {
                if ledger.tasks.contains_key(&task.id) {
                    continue;
                }
                let task = task.into_task(router_job_id.clone(), now_ms);
                ledger.tasks.insert(task.id.clone(), task);
                changed = true;
            }
            changed
        }
        WorkRequest::Bind {
            router_job_id,
            task,
            worker_job_id,
            ..
        } => {
            let mut task = task.into_task(router_job_id, now_ms);
            task.worker_job_id = Some(worker_job_id);
            task.attempts = 1;
            task.set(TaskStatus::Running, None, now_ms);
            ledger.tasks.insert(task.id.clone(), task);
            true
        }
        WorkRequest::Cancel {
            task_id, reason, ..
        } => match ledger.tasks.get_mut(&task_id) {
            Some(task) if task.status != TaskStatus::Cancelled => {
                task.set(TaskStatus::Cancelled, reason, now_ms);
                true
            }
            _ => false,
        },
    }
}

/// Where a task's worker stands, as the caller reads its job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerPhase {
    /// Queued, running, or waiting for an answer.
    Active,
    /// Its turn ended.
    Finished,
    Failed(String),
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerView {
    pub job_id: String,
    pub phase: WorkerPhase,
}

/// What a pass does to the world, injected so the rules can be tested
/// without a session host.
pub trait Hands {
    /// Start a worker on a task; its job id, or why it could not start.
    fn launch_worker(&mut self, task: &Task) -> Result<String, String>;
    /// Start the acceptance check of a task whose worker says it finished.
    fn launch_verifier(&mut self, task: &Task) -> Result<String, String>;
    /// What a finished job last said.
    fn last_reply(&mut self, job_id: &str) -> Option<String>;
    /// Give a task's worker the acceptance check's findings as its next turn.
    fn rework(&mut self, task: &Task, round: u32, findings: &str) -> Result<(), String>;
}

/// What a pass did that the router should hear about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskNotice {
    pub task_id: String,
    pub title: String,
    pub router_job_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct PassOutcome {
    pub changed: bool,
    pub notices: Vec<TaskNotice>,
}

/// A check's reading of a task: passed or failed, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    pub reason: String,
}

/// The verdict a check's reply ends with: its last `VERDICT: PASS …` or
/// `VERDICT: FAIL …` line.
pub fn parse_verdict(reply: &str) -> Option<Verdict> {
    reply.lines().rev().find_map(|line| {
        let line = line.trim().trim_start_matches(['*', '`', '#', '>', ' ']);
        let rest = strip_prefix_ignore_case(line, "VERDICT")?;
        let rest = rest.trim_start_matches([':', '：', ' ', '*']).trim();
        let (passed, rest) = if let Some(rest) = strip_prefix_ignore_case(rest, "PASS") {
            (true, rest)
        } else if let Some(rest) = strip_prefix_ignore_case(rest, "FAIL") {
            (false, rest)
        } else {
            return None;
        };
        let reason = rest
            .trim_start_matches(['*', '`'])
            .trim_start_matches([' ', ':', '：', '-', '—', '–'])
            .trim_end_matches(['*', '`'])
            .trim()
            .to_string();
        Some(Verdict { passed, reason })
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// One pass at `now_ms`: follow every task's worker and acceptance check,
/// then dispatch what is ready while fewer than `max_running` tasks are being
/// worked on or checked.
pub fn run_pass(
    ledger: &mut Ledger,
    workers: &[WorkerView],
    now_ms: u64,
    max_running: usize,
    hands: &mut dyn Hands,
) -> PassOutcome {
    let mut outcome = PassOutcome::default();
    for task in ledger.tasks.values_mut() {
        follow(task, workers, now_ms, hands, &mut outcome);
    }
    dispatch_ready(ledger, now_ms, max_running, hands, &mut outcome);
    outcome
}

fn notice(outcome: &mut PassOutcome, task: &Task, text: String) {
    outcome.notices.push(TaskNotice {
        task_id: task.id.clone(),
        title: task.title.clone(),
        router_job_id: task.router_job_id.clone(),
        text,
    });
}

fn phase_of(workers: &[WorkerView], job_id: &str) -> Option<WorkerPhase> {
    workers
        .iter()
        .find(|worker| worker.job_id == job_id)
        .map(|worker| worker.phase.clone())
}

fn stuck(task: &mut Task, note: String, now_ms: u64, outcome: &mut PassOutcome) {
    task.set(TaskStatus::Stuck, Some(note.clone()), now_ms);
    outcome.changed = true;
    notice(outcome, task, format!("is stuck: {note}"));
}

/// Move one task along by what its worker and its check are doing.
///
/// A worker that is busy again (continued by the router, brought back after
/// a crash) takes its task back to running, whatever it stood at; the check
/// it was under, if any, no longer counts.
fn follow(
    task: &mut Task,
    workers: &[WorkerView],
    now_ms: u64,
    hands: &mut dyn Hands,
    outcome: &mut PassOutcome,
) {
    if matches!(task.status, TaskStatus::Cancelled | TaskStatus::Planned) {
        return;
    }
    let Some(worker_id) = task.worker_job_id.clone() else {
        return;
    };
    let worker = phase_of(workers, &worker_id);
    if worker == Some(WorkerPhase::Active) {
        if task.status != TaskStatus::Running {
            task.verifier_job_id = None;
            task.set(TaskStatus::Running, None, now_ms);
            outcome.changed = true;
        }
        return;
    }
    match task.status {
        TaskStatus::Running => match worker {
            Some(WorkerPhase::Finished) => start_check(task, now_ms, hands, outcome, 1),
            Some(WorkerPhase::Failed(error)) => stuck(
                task,
                format!("its worker {worker_id} failed: {error}"),
                now_ms,
                outcome,
            ),
            Some(WorkerPhase::Stopped) => {
                task.set(
                    TaskStatus::Cancelled,
                    Some(format!("its worker {worker_id} was stopped")),
                    now_ms,
                );
                outcome.changed = true;
            }
            None => stuck(
                task,
                format!("its worker {worker_id} is gone"),
                now_ms,
                outcome,
            ),
            Some(WorkerPhase::Active) => unreachable!("handled above"),
        },
        TaskStatus::Verifying => follow_check(task, workers, now_ms, hands, outcome),
        // Done or stuck, and its worker idle: nothing new.
        _ => {}
    }
}

/// Start (or start again) the acceptance check of a task whose worker says
/// it finished.
fn start_check(
    task: &mut Task,
    now_ms: u64,
    hands: &mut dyn Hands,
    outcome: &mut PassOutcome,
    attempt: u32,
) {
    match hands.launch_verifier(task) {
        Ok(job_id) => {
            task.verifier_job_id = Some(job_id.clone());
            task.verifier_attempts = attempt;
            task.set(TaskStatus::Verifying, None, now_ms);
            task.record(now_ms, format!("acceptance check by {job_id}"));
            outcome.changed = true;
        }
        Err(error) => stuck(
            task,
            format!("its acceptance check could not start: {error}"),
            now_ms,
            outcome,
        ),
    }
}

fn follow_check(
    task: &mut Task,
    workers: &[WorkerView],
    now_ms: u64,
    hands: &mut dyn Hands,
    outcome: &mut PassOutcome,
) {
    let Some(checker) = task.verifier_job_id.clone() else {
        return start_check(task, now_ms, hands, outcome, 1);
    };
    let verdict = match phase_of(workers, &checker) {
        Some(WorkerPhase::Active) => return,
        Some(WorkerPhase::Finished) => hands
            .last_reply(&checker)
            .as_deref()
            .and_then(parse_verdict),
        _ => None,
    };
    let Some(verdict) = verdict else {
        // The check broke down or gave no verdict: that says nothing about
        // the work, so the check runs again rather than the work.
        if task.verifier_attempts < MAX_CHECK_ATTEMPTS {
            return start_check(task, now_ms, hands, outcome, task.verifier_attempts + 1);
        }
        return stuck(
            task,
            "its acceptance check gave no verdict, twice".to_string(),
            now_ms,
            outcome,
        );
    };
    task.verdicts.push(TaskVerdict {
        at_ms: now_ms,
        passed: verdict.passed,
        reason: verdict.reason.clone(),
        verifier_job_id: checker,
    });
    task.verifier_job_id = None;
    if verdict.passed {
        task.set(TaskStatus::Done, None, now_ms);
        outcome.changed = true;
        let evidence = if verdict.reason.is_empty() {
            String::new()
        } else {
            format!(": {}", verdict.reason)
        };
        notice(
            outcome,
            task,
            format!("is done and passed its acceptance check{evidence}"),
        );
        return;
    }
    if task.rework_rounds >= MAX_REWORK_ROUNDS {
        return stuck(
            task,
            format!(
                "it failed its acceptance check {} times; the last time: {}",
                task.rework_rounds + 1,
                verdict.reason
            ),
            now_ms,
            outcome,
        );
    }
    task.rework_rounds += 1;
    match hands.rework(task, task.rework_rounds, &verdict.reason) {
        Ok(()) => {
            task.set(
                TaskStatus::Running,
                Some(format!(
                    "rework {} of {MAX_REWORK_ROUNDS}: {}",
                    task.rework_rounds, verdict.reason
                )),
                now_ms,
            );
            outcome.changed = true;
        }
        Err(error) => stuck(
            task,
            format!("its acceptance check failed and the rework could not be queued: {error}"),
            now_ms,
            outcome,
        ),
    }
}

fn dispatch_ready(
    ledger: &mut Ledger,
    now_ms: u64,
    max_running: usize,
    hands: &mut dyn Hands,
    outcome: &mut PassOutcome,
) {
    let statuses: BTreeMap<String, TaskStatus> = ledger
        .tasks
        .iter()
        .map(|(id, task)| (id.clone(), task.status))
        .collect();
    let mut running = statuses
        .values()
        .filter(|status| matches!(status, TaskStatus::Running | TaskStatus::Verifying))
        .count();
    let mut planned: Vec<&mut Task> = ledger
        .tasks
        .values_mut()
        .filter(|task| task.status == TaskStatus::Planned)
        .collect();
    planned.sort_by(|a, b| (a.created_at_ms, &a.id).cmp(&(b.created_at_ms, &b.id)));
    for task in planned {
        // What it waits for, and whether any of that can no longer happen.
        let mut waiting = Vec::new();
        let mut blocked = Vec::new();
        for dep in &task.depends_on {
            match statuses.get(dep) {
                Some(TaskStatus::Done) => {}
                Some(status @ (TaskStatus::Stuck | TaskStatus::Cancelled)) => {
                    blocked.push(format!("{dep} ({})", status.as_str()))
                }
                Some(_) => waiting.push(dep.clone()),
                None => blocked.push(format!("{dep} (no such task)")),
            }
        }
        if !blocked.is_empty() {
            let note = format!("blocked by {}", blocked.join(", "));
            if task.note.as_deref() != Some(note.as_str()) {
                task.note = Some(note.clone());
                task.record(now_ms, note.clone());
                outcome.changed = true;
                notice(outcome, task, format!("is {note}"));
            }
            continue;
        }
        if !waiting.is_empty() {
            let note = format!("waiting for {}", waiting.join(", "));
            if task.note.as_deref() != Some(note.as_str()) {
                task.note = Some(note);
                outcome.changed = true;
            }
            continue;
        }
        if running >= max_running {
            continue;
        }
        match hands.launch_worker(task) {
            Ok(job_id) => {
                task.worker_job_id = Some(job_id.clone());
                task.attempts += 1;
                task.set(TaskStatus::Running, None, now_ms);
                task.record(now_ms, format!("dispatched to {job_id}"));
                running += 1;
                outcome.changed = true;
            }
            Err(error) => stuck(
                task,
                format!("could not be dispatched: {error}"),
                now_ms,
                outcome,
            ),
        }
    }
}

/// What every acceptance check is told. The same for every task: the check
/// is a fixed step, not something a plan chooses or tunes.
pub fn verifier_brief(task: &Task) -> String {
    let done_when = if task.done_when.trim().is_empty() {
        "(not given: judge by what the task asks for)".to_string()
    } else {
        task.done_when.trim().to_string()
    };
    format!(
        "You are the acceptance check for a task that another worker says it has finished. You \
         did not do the work; your job is to judge it.\n\n\
         Task {id}: {title}\n\
         Done when: {done_when}\n\n\
         What the worker was asked:\n{goal}\n\n\
         Check the actual state of the project, not the worker's account of it: read what \
         changed (for example `git status` and `git diff`), run the tests or commands that show \
         whether it holds, and look at what was produced. Hold the work to what \"done when\" \
         says, no more and no less. Do not fix anything and do not change the project's files; \
         put any scratch files outside the project.\n\n\
         End your reply with exactly one line, either\n\
         VERDICT: PASS — <the evidence, in one line>\n\
         or\n\
         VERDICT: FAIL — <what is missing or wrong, concretely enough to fix>",
        id = task.id,
        title = task.title,
        goal = task.goal.trim(),
    )
}

/// The turn a task's worker gets when its acceptance check failed.
pub fn rework_message(round: u32, findings: &str) -> String {
    format!(
        "[Acceptance check failed: rework {round} of {MAX_REWORK_ROUNDS}]\n\
         An independent check of your task found this:\n{findings}\n\n\
         Fix it, check it yourself, and report again with the evidence."
    )
}

/// Whether any task still needs the supervisor: one waiting to be
/// dispatched, being worked on, or being checked.
pub fn has_open_tasks(ledger: &Ledger) -> bool {
    ledger.tasks.values().any(|task| {
        matches!(
            task.status,
            TaskStatus::Planned | TaskStatus::Running | TaskStatus::Verifying
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(id: &str, deps: &[&str]) -> NewTask {
        NewTask {
            id: id.into(),
            title: format!("task {id}"),
            project: "/p".into(),
            goal: "do it".into(),
            done_when: "it is done".into(),
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            isolate: false,
            model_profile: None,
        }
    }

    fn plan(ledger: &mut Ledger, tasks: Vec<NewTask>, now: u64) {
        apply_request(
            ledger,
            WorkRequest::Plan {
                router_job_id: Some("r".into()),
                tasks,
                requested_at_ms: now,
            },
            now,
        );
    }

    fn worker(id: &str, phase: WorkerPhase) -> WorkerView {
        WorkerView {
            job_id: id.into(),
            phase,
        }
    }

    /// Workers are `w-<task>`, checks `c-<task>-<n>`; replies and rework
    /// are recorded.
    #[derive(Default)]
    struct Fake {
        launched: Vec<String>,
        checks: Vec<String>,
        replies: BTreeMap<String, String>,
        reworked: Vec<(String, u32, String)>,
        fail_launch: Option<String>,
    }

    impl Hands for Fake {
        fn launch_worker(&mut self, task: &Task) -> Result<String, String> {
            if let Some(error) = &self.fail_launch {
                return Err(error.clone());
            }
            self.launched.push(task.id.clone());
            Ok(format!("w-{}", task.id))
        }
        fn launch_verifier(&mut self, task: &Task) -> Result<String, String> {
            let id = format!("c-{}-{}", task.id, self.checks.len() + 1);
            self.checks.push(id.clone());
            Ok(id)
        }
        fn last_reply(&mut self, job_id: &str) -> Option<String> {
            self.replies.get(job_id).cloned()
        }
        fn rework(&mut self, task: &Task, round: u32, findings: &str) -> Result<(), String> {
            self.reworked
                .push((task.id.clone(), round, findings.to_string()));
            Ok(())
        }
    }

    #[test]
    fn a_verdict_is_the_last_verdict_line_in_any_markup() {
        assert_eq!(
            parse_verdict("checked\nVERDICT: PASS — 6 passed"),
            Some(Verdict {
                passed: true,
                reason: "6 passed".into()
            })
        );
        assert_eq!(
            parse_verdict("**Verdict: FAIL** - no test for mul"),
            Some(Verdict {
                passed: false,
                reason: "no test for mul".into()
            })
        );
        assert_eq!(
            parse_verdict("VERDICT: FAIL: x\nlater\n`VERDICT：PASS`"),
            Some(Verdict {
                passed: true,
                reason: String::new()
            })
        );
        assert_eq!(parse_verdict("looks good to me"), None);
        assert_eq!(parse_verdict("VERDICT: maybe"), None);
    }

    #[test]
    fn a_plan_runs_in_dependency_order_within_the_limit_and_each_task_is_checked() {
        let mut ledger = Ledger::default();
        plan(
            &mut ledger,
            vec![new("a", &[]), new("b", &[]), new("c", &["a", "b"])],
            1,
        );
        let mut hands = Fake::default();
        run_pass(&mut ledger, &[], 2, 1, &mut hands);
        assert_eq!(hands.launched, ["a"], "one at a time");
        assert_eq!(ledger.tasks["c"].note.as_deref(), Some("waiting for a, b"));

        // a's worker finishes: it is checked before it counts, and the check
        // holds the slot.
        run_pass(
            &mut ledger,
            &[worker("w-a", WorkerPhase::Finished)],
            3,
            1,
            &mut hands,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Verifying);
        assert_eq!(hands.checks, ["c-a-1"]);
        assert_eq!(hands.launched, ["a"]);

        hands
            .replies
            .insert("c-a-1".into(), "VERDICT: PASS — tests pass".into());
        let workers = [
            worker("w-a", WorkerPhase::Finished),
            worker("c-a-1", WorkerPhase::Finished),
        ];
        let outcome = run_pass(&mut ledger, &workers, 4, 1, &mut hands);
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Done);
        assert_eq!(hands.launched, ["a", "b"]);
        assert_eq!(
            outcome.notices[0].text,
            "is done and passed its acceptance check: tests pass"
        );
        assert!(has_open_tasks(&ledger));
    }

    #[test]
    fn a_failed_check_sends_the_work_back_until_the_rounds_run_out() {
        let mut ledger = Ledger::default();
        plan(&mut ledger, vec![new("a", &[])], 1);
        let mut hands = Fake::default();
        run_pass(&mut ledger, &[], 2, 3, &mut hands);
        let mut now = 3;
        for round in 1..=MAX_REWORK_ROUNDS + 1 {
            // The worker says it finished; the check runs and fails.
            run_pass(
                &mut ledger,
                &[worker("w-a", WorkerPhase::Finished)],
                now,
                3,
                &mut hands,
            );
            let check = hands.checks.last().unwrap().clone();
            hands.replies.insert(
                check.clone(),
                format!("VERDICT: FAIL — still wrong {round}"),
            );
            let workers = [
                worker("w-a", WorkerPhase::Finished),
                worker(&check, WorkerPhase::Finished),
            ];
            let outcome = run_pass(&mut ledger, &workers, now + 1, 3, &mut hands);
            if round <= MAX_REWORK_ROUNDS {
                assert_eq!(ledger.tasks["a"].status, TaskStatus::Running);
                assert!(outcome.notices.is_empty(), "the router is not bothered");
                // Its worker takes the findings up.
                run_pass(
                    &mut ledger,
                    &[worker("w-a", WorkerPhase::Active)],
                    now + 2,
                    3,
                    &mut hands,
                );
            } else {
                assert_eq!(ledger.tasks["a"].status, TaskStatus::Stuck);
                assert!(outcome.notices[0]
                    .text
                    .contains("failed its acceptance check 4 times"));
            }
            now += 10;
        }
        assert_eq!(hands.reworked.len() as u32, MAX_REWORK_ROUNDS);
        assert_eq!(hands.reworked[0], ("a".into(), 1, "still wrong 1".into()));
        assert_eq!(
            ledger.tasks["a"].verdicts.len() as u32,
            MAX_REWORK_ROUNDS + 1
        );
    }

    #[test]
    fn a_check_that_gives_no_verdict_runs_again_and_then_the_task_is_stuck() {
        let mut ledger = Ledger::default();
        plan(&mut ledger, vec![new("a", &[])], 1);
        let mut hands = Fake::default();
        run_pass(&mut ledger, &[], 2, 3, &mut hands);
        run_pass(
            &mut ledger,
            &[worker("w-a", WorkerPhase::Finished)],
            3,
            3,
            &mut hands,
        );
        hands.replies.insert("c-a-1".into(), "it seems fine".into());
        let workers = [
            worker("w-a", WorkerPhase::Finished),
            worker("c-a-1", WorkerPhase::Finished),
            worker("c-a-2", WorkerPhase::Failed("crashed".into())),
        ];
        run_pass(&mut ledger, &workers, 4, 3, &mut hands);
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Verifying);
        assert_eq!(hands.checks, ["c-a-1", "c-a-2"]);
        let outcome = run_pass(&mut ledger, &workers, 5, 3, &mut hands);
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Stuck);
        assert!(outcome.notices[0].text.contains("gave no verdict"));
        assert!(hands.reworked.is_empty(), "the work was never judged");
    }

    #[test]
    fn a_task_whose_dependency_cannot_finish_is_blocked_and_said_once() {
        let mut ledger = Ledger::default();
        plan(&mut ledger, vec![new("a", &[]), new("b", &["a"])], 1);
        let mut hands = Fake::default();
        run_pass(&mut ledger, &[], 2, 3, &mut hands);
        let failed = [worker("w-a", WorkerPhase::Failed("quota".into()))];
        let outcome = run_pass(&mut ledger, &failed, 3, 3, &mut hands);
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Stuck);
        assert_eq!(
            ledger.tasks["b"].note.as_deref(),
            Some("blocked by a (stuck)")
        );
        let texts: Vec<_> = outcome.notices.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "is stuck: its worker w-a failed: quota",
                "is blocked by a (stuck)"
            ]
        );
        let again = run_pass(&mut ledger, &failed, 4, 3, &mut hands);
        assert!(again.notices.is_empty(), "{:?}", again.notices);
    }

    #[test]
    fn a_task_follows_its_worker_back_to_work_and_a_stop_cancels_it() {
        let mut ledger = Ledger::default();
        apply_request(
            &mut ledger,
            WorkRequest::Bind {
                router_job_id: Some("r".into()),
                task: new("a", &[]),
                worker_job_id: "w".into(),
                requested_at_ms: 1,
            },
            1,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Running);
        let mut hands = Fake::default();
        run_pass(
            &mut ledger,
            &[worker("w", WorkerPhase::Finished)],
            2,
            3,
            &mut hands,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Verifying);
        // The router continues the worker mid-check: the check no longer counts.
        run_pass(
            &mut ledger,
            &[worker("w", WorkerPhase::Active)],
            3,
            3,
            &mut hands,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Running);
        assert_eq!(ledger.tasks["a"].verifier_job_id, None);
        run_pass(
            &mut ledger,
            &[worker("w", WorkerPhase::Stopped)],
            4,
            3,
            &mut hands,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Cancelled);
        assert!(!has_open_tasks(&ledger));
        assert!(hands.launched.is_empty(), "it was already running");
    }

    #[test]
    fn a_launch_that_fails_leaves_the_task_stuck_with_the_reason() {
        let mut ledger = Ledger::default();
        plan(&mut ledger, vec![new("a", &[])], 1);
        let mut hands = Fake {
            fail_launch: Some("not a project directory".into()),
            ..Fake::default()
        };
        let outcome = run_pass(&mut ledger, &[], 2, 3, &mut hands);
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Stuck);
        assert_eq!(outcome.notices.len(), 1);
        assert!(outcome.notices[0].text.contains("not a project directory"));
    }

    #[test]
    fn requests_round_trip_through_their_files_once_and_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = WorkRequest::Plan {
            router_job_id: None,
            tasks: vec![new("a", &[])],
            requested_at_ms: 5,
        };
        let second = WorkRequest::Cancel {
            task_id: "a".into(),
            reason: Some("not needed".into()),
            requested_at_ms: 9,
        };
        file_request(dir.path(), &second).unwrap();
        file_request(dir.path(), &first).unwrap();
        let taken = take_requests(dir.path());
        assert_eq!(taken, [first, second]);
        assert!(take_requests(dir.path()).is_empty());

        let mut ledger = Ledger::default();
        for request in taken {
            apply_request(&mut ledger, request, 10);
        }
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Cancelled);
        assert_eq!(ledger.tasks["a"].note.as_deref(), Some("not needed"));
        let path = ledger_path(dir.path());
        ledger.save(&path).unwrap();
        assert_eq!(Ledger::load(&path).unwrap().tasks, ledger.tasks);
    }

    #[test]
    fn task_ids_are_distinct() {
        let ids: std::collections::BTreeSet<_> = (0..100).map(|_| new_task_id(42)).collect();
        assert_eq!(ids.len(), 100);
        assert!(ids.iter().all(|id| id.starts_with("t-")));
    }
}
