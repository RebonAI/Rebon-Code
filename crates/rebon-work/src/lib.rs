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
//! Launching a worker and reading jobs are the caller's. This crate names no
//! session host.

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
    /// Its worker finished.
    Done,
    /// It cannot go on without a decision: its worker failed or is gone,
    /// or it could not be dispatched.
    Stuck,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
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

    /// The task `worker_job_id` works on, if any.
    pub fn task_of_worker(&self, worker_job_id: &str) -> Option<&Task> {
        self.tasks
            .values()
            .find(|task| task.worker_job_id.as_deref() == Some(worker_job_id))
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

/// Start a worker for a task; its job id, or why it could not start.
pub type LaunchTask<'a> = dyn FnMut(&Task) -> Result<String, String> + 'a;

/// What a pass did that the router should hear about, beyond what its
/// workers' own digests say.
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

/// One pass at `now_ms`: follow every task's worker, then dispatch what is
/// ready while fewer than `max_running` tasks run.
pub fn run_pass(
    ledger: &mut Ledger,
    workers: &[WorkerView],
    now_ms: u64,
    max_running: usize,
    launch: &mut LaunchTask<'_>,
) -> PassOutcome {
    let mut outcome = PassOutcome::default();
    follow_workers(ledger, workers, now_ms, &mut outcome);
    dispatch_ready(ledger, now_ms, max_running, launch, &mut outcome);
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

/// A task follows its worker: a worker that is busy again (continued by
/// the router, brought back after a crash) takes its task back to running.
fn follow_workers(
    ledger: &mut Ledger,
    workers: &[WorkerView],
    now_ms: u64,
    outcome: &mut PassOutcome,
) {
    for task in ledger.tasks.values_mut() {
        if task.status == TaskStatus::Cancelled || task.status == TaskStatus::Planned {
            continue;
        }
        let Some(worker_id) = task.worker_job_id.clone() else {
            continue;
        };
        let phase = workers
            .iter()
            .find(|worker| worker.job_id == worker_id)
            .map(|worker| worker.phase.clone());
        let next = match phase {
            None => (
                TaskStatus::Stuck,
                Some(format!("its worker {worker_id} is gone")),
            ),
            Some(WorkerPhase::Active) => (TaskStatus::Running, None),
            Some(WorkerPhase::Finished) => (TaskStatus::Done, None),
            Some(WorkerPhase::Failed(error)) => (
                TaskStatus::Stuck,
                Some(format!("its worker {worker_id} failed: {error}")),
            ),
            Some(WorkerPhase::Stopped) => (
                TaskStatus::Cancelled,
                Some(format!("its worker {worker_id} was stopped")),
            ),
        };
        if (task.status, &task.note) == (next.0, &next.1) {
            continue;
        }
        task.set(next.0, next.1.clone(), now_ms);
        outcome.changed = true;
        // A worker that is gone or failed says nothing itself that names the
        // task; the router hears it here.
        if next.0 == TaskStatus::Stuck {
            if let Some(note) = next.1 {
                notice(outcome, task, format!("is stuck: {note}"));
            }
        }
    }
}

fn dispatch_ready(
    ledger: &mut Ledger,
    now_ms: u64,
    max_running: usize,
    launch: &mut LaunchTask<'_>,
    outcome: &mut PassOutcome,
) {
    let statuses: BTreeMap<String, TaskStatus> = ledger
        .tasks
        .iter()
        .map(|(id, task)| (id.clone(), task.status))
        .collect();
    let mut running = statuses
        .values()
        .filter(|status| **status == TaskStatus::Running)
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
        match launch(task) {
            Ok(job_id) => {
                task.worker_job_id = Some(job_id.clone());
                task.attempts += 1;
                task.set(TaskStatus::Running, None, now_ms);
                task.record(now_ms, format!("dispatched to {job_id}"));
                running += 1;
            }
            Err(error) => {
                let note = format!("could not be dispatched: {error}");
                task.set(TaskStatus::Stuck, Some(note.clone()), now_ms);
                notice(outcome, task, format!("is stuck: {note}"));
            }
        }
        outcome.changed = true;
    }
}

/// Whether any task still needs the supervisor: one waiting to be
/// dispatched, or one whose worker is on it.
pub fn has_open_tasks(ledger: &Ledger) -> bool {
    ledger
        .tasks
        .values()
        .any(|task| matches!(task.status, TaskStatus::Planned | TaskStatus::Running))
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

    #[test]
    fn a_plan_runs_in_dependency_order_within_the_limit() {
        let mut ledger = Ledger::default();
        plan(
            &mut ledger,
            vec![new("a", &[]), new("b", &[]), new("c", &["a", "b"])],
            1,
        );
        let launched = std::cell::RefCell::new(Vec::new());
        let mut launch = |task: &Task| {
            launched.borrow_mut().push(task.id.clone());
            Ok(format!("w-{}", task.id))
        };
        run_pass(&mut ledger, &[], 2, 1, &mut launch);
        assert_eq!(*launched.borrow(), ["a"], "one at a time");
        assert_eq!(ledger.tasks["c"].note.as_deref(), Some("waiting for a, b"));

        let workers = [worker("w-a", WorkerPhase::Finished)];
        run_pass(&mut ledger, &workers, 3, 1, &mut launch);
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Done);
        assert_eq!(*launched.borrow(), ["a", "b"]);

        let workers = [
            worker("w-a", WorkerPhase::Finished),
            worker("w-b", WorkerPhase::Finished),
        ];
        run_pass(&mut ledger, &workers, 4, 1, &mut launch);
        assert_eq!(*launched.borrow(), ["a", "b", "c"]);
        assert_eq!(ledger.tasks["c"].status, TaskStatus::Running);
        assert!(has_open_tasks(&ledger));
    }

    #[test]
    fn a_task_whose_dependency_cannot_finish_is_blocked_and_said_once() {
        let mut ledger = Ledger::default();
        plan(&mut ledger, vec![new("a", &[]), new("b", &["a"])], 1);
        let mut launch = |task: &Task| Ok(format!("w-{}", task.id));
        run_pass(&mut ledger, &[], 2, 3, &mut launch);
        let failed = [worker("w-a", WorkerPhase::Failed("quota".into()))];
        let outcome = run_pass(&mut ledger, &failed, 3, 3, &mut launch);
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
        let again = run_pass(&mut ledger, &failed, 4, 3, &mut launch);
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
        let mut launch = |_: &Task| -> Result<String, String> { panic!("already running") };
        run_pass(
            &mut ledger,
            &[worker("w", WorkerPhase::Finished)],
            2,
            3,
            &mut launch,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Done);
        run_pass(
            &mut ledger,
            &[worker("w", WorkerPhase::Active)],
            3,
            3,
            &mut launch,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Running, "continued");
        run_pass(
            &mut ledger,
            &[worker("w", WorkerPhase::Stopped)],
            4,
            3,
            &mut launch,
        );
        assert_eq!(ledger.tasks["a"].status, TaskStatus::Cancelled);
        assert!(!has_open_tasks(&ledger));
    }

    #[test]
    fn a_launch_that_fails_leaves_the_task_stuck_with_the_reason() {
        let mut ledger = Ledger::default();
        plan(&mut ledger, vec![new("a", &[])], 1);
        let mut launch = |_: &Task| Err("not a project directory".to_string());
        let outcome = run_pass(&mut ledger, &[], 2, 3, &mut launch);
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
