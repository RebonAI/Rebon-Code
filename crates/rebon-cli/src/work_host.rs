//! The router's task ledger (`rebon_work`), run by the background supervisor.
//!
//! On its tick, every couple of seconds or at once when a request waits, the
//! supervisor applies the plans and cancellations the router's tools filed,
//! follows each task's worker and acceptance check, and dispatches the tasks
//! that are ready, within the limit on automatic turns. It is the ledger's only writer. What a pass
//! notices that no worker's own digest would say (a task stuck, a task blocked
//! by another) goes into the router's next digest.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundStore, STALE_PID_EXIT_ERROR,
};
use rebon_work::{Hands, Ledger, Task, TaskNotice, WorkerPhase, WorkerView};

const PASS_INTERVAL_MS: u64 = 2_000;

pub(crate) struct WorkHost {
    home: PathBuf,
    /// Loaded on the first pass and kept: this process is the only writer.
    ledger: RefCell<Option<Ledger>>,
    last_pass_ms: Cell<Option<u64>>,
    /// Whether the last pass left a task open, which keeps the supervisor up.
    resident: Cell<bool>,
}

impl WorkHost {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self {
            home,
            ledger: RefCell::new(None),
            last_pass_ms: Cell::new(None),
            resident: Cell::new(false),
        }
    }

    /// The task each worker works on, for the digest.
    pub(crate) fn task_of_worker(&self, worker_job_id: &str) -> Option<String> {
        self.ledger
            .borrow()
            .as_ref()?
            .task_of_worker(worker_job_id)
            .map(|task| task.id.clone())
    }

    /// One tick. `true` while a task is open; the notices are for the router.
    pub(crate) fn tick(
        &self,
        store: &BackgroundStore,
        jobs: &[BackgroundJobState],
        now_ms: u64,
        max_running: usize,
    ) -> (bool, Vec<TaskNotice>) {
        let due = self
            .last_pass_ms
            .get()
            .is_none_or(|last| now_ms.saturating_sub(last) >= PASS_INTERVAL_MS);
        let requested = std::fs::read_dir(rebon_work::requests_dir(&self.home))
            .is_ok_and(|mut entries| entries.next().is_some());
        if !due && !requested {
            return (self.resident.get(), Vec::new());
        }
        self.last_pass_ms.set(Some(now_ms));
        let mut slot = self.ledger.borrow_mut();
        if slot.is_none() {
            match Ledger::load(&rebon_work::ledger_path(&self.home)) {
                Ok(ledger) => *slot = Some(ledger),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "rebon: the task ledger is unreadable; leaving it alone");
                    return (false, Vec::new());
                }
            }
        }
        let ledger = slot.as_mut().expect("just loaded");
        let mut changed = false;
        let workers: Vec<WorkerView> = jobs.iter().map(worker_view).collect();
        let mut hands = HostHands { store, jobs };
        for request in rebon_work::take_requests(&self.home) {
            changed |= match request {
                event @ rebon_work::WorkRequest::Event { .. } => {
                    rebon_work::route_event(ledger, event, &workers, now_ms, &mut hands)
                }
                request => rebon_work::apply_request(ledger, request, now_ms),
            };
        }
        let outcome = rebon_work::run_pass(ledger, &workers, now_ms, max_running, &mut hands);
        if changed || outcome.changed {
            if let Err(error) = ledger.save(&rebon_work::ledger_path(&self.home)) {
                tracing::warn!(error = %format!("{error:#}"), "rebon: could not save the task ledger");
            }
        }
        self.resident.set(rebon_work::has_open_tasks(ledger));
        (self.resident.get(), outcome.notices)
    }
}

/// Commit everything in `worktree` with `message`. The short commit, or
/// `None` when nothing had changed. A repository without an identity of its
/// own commits as Rebon.
fn freeze_worktree(worktree: &std::path::Path, message: &str) -> Result<Option<String>, String> {
    let git = |args: &[&str]| -> Result<std::process::Output, String> {
        let mut command = std::process::Command::new("git");
        command.arg("-C").arg(worktree).args(args);
        command.stdin(std::process::Stdio::null());
        command
            .output()
            .map_err(|error| format!("git {}: {error}", args.join(" ")))
    };
    let succeed = |output: std::process::Output, what: &str| -> Result<String, String> {
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(format!(
                "git {what} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    };
    succeed(git(&["add", "-A"])?, "add")?;
    let status = succeed(git(&["status", "--porcelain"])?, "status")?;
    if status.is_empty() {
        return Ok(None);
    }
    let has_identity = git(&["config", "user.email"])
        .map(|output| output.status.success() && !output.stdout.trim_ascii().is_empty())
        .unwrap_or(false);
    let mut args = Vec::new();
    if !has_identity {
        args.extend(["-c", "user.name=Rebon", "-c", "user.email=rebon@localhost"]);
    }
    args.extend(["commit", "--no-verify", "-q", "-m", message]);
    succeed(git(&args)?, "commit")?;
    succeed(git(&["rev-parse", "--short", "HEAD"])?, "rev-parse").map(Some)
}

fn worker_view(job: &BackgroundJobState) -> WorkerView {
    let phase = match job.process.status {
        BackgroundJobStatus::Queued
        | BackgroundJobStatus::Running
        | BackgroundJobStatus::NeedsInput => WorkerPhase::Active,
        BackgroundJobStatus::Idle | BackgroundJobStatus::Succeeded => WorkerPhase::Finished,
        BackgroundJobStatus::Failed => {
            let error = job
                .outcome
                .error
                .clone()
                .unwrap_or_else(|| "no error recorded".to_string());
            // A crash may yet be recovered (`crate::autonomy`): say so, so
            // the router does not start the same work twice.
            WorkerPhase::Failed(if error == STALE_PID_EXIT_ERROR {
                format!("{error} (it may be brought back automatically)")
            } else {
                error
            })
        }
        BackgroundJobStatus::Stopped => WorkerPhase::Stopped,
    };
    WorkerView {
        job_id: job.identity.job_id.clone(),
        phase,
        worktree: job.workspace.worktree_path.clone(),
    }
}

/// What a pass does to the world: start jobs, read what they said, queue
/// rework.
struct HostHands<'a> {
    store: &'a BackgroundStore,
    jobs: &'a [BackgroundJobState],
}

impl HostHands<'_> {
    fn job(&self, job_id: &str) -> Option<&BackgroundJobState> {
        self.jobs.iter().find(|job| job.identity.job_id == job_id)
    }

    /// The router that planned `task`, which its workers work under.
    fn router(&self, task: &Task) -> Result<&BackgroundJobState, String> {
        let router_id = task
            .router_job_id
            .as_deref()
            .ok_or("the task has no router to work under")?;
        self.job(router_id)
            .ok_or_else(|| format!("its router {router_id} is gone"))
    }
}

impl Hands for HostHands<'_> {
    fn launch_worker(&mut self, task: &Task, worktree: Option<&str>) -> Result<String, String> {
        let router = self.router(task)?;
        // Carrying on in a worktree means working there as it is, with the
        // work before it committed; the worker is told whose it is.
        let (project, isolate, brief) = match worktree {
            Some(worktree) => (
                worktree.to_string(),
                false,
                format!(
                    "You carry on in the worktree {worktree}, where the work this task depends on \
                     is already committed. Build on it; do not start over.\n\n{}",
                    task.goal
                ),
            ),
            None => (task.project.clone(), task.isolate, task.goal.clone()),
        };
        let request = rebon_tool::WorkerDispatch {
            project,
            title: task.title.clone(),
            brief,
            done_when: task.done_when.clone(),
            isolate,
            model_profile: task.model_profile.clone(),
            worker: None,
        };
        crate::session::worker_control::launch_worker(self.store, router, &request, Some(&task.id))
            .map(|worker| worker.worker_id)
    }

    /// The check looks where the work is: the worker's worktree, if it has
    /// one, else the project.
    fn launch_verifier(&mut self, task: &Task) -> Result<String, String> {
        let router = self.router(task)?;
        let cwd = task
            .worker_job_id
            .as_deref()
            .and_then(|worker| self.job(worker))
            .and_then(|worker| worker.workspace.worktree_path.clone())
            .unwrap_or_else(|| task.project.clone());
        crate::session::worker_control::launch_verifier(self.store, router, task, &cwd)
    }

    fn freeze(&mut self, task: &Task, worktree: &str) -> Result<Option<String>, String> {
        freeze_worktree(
            std::path::Path::new(worktree),
            &format!("{}: {}", task.id, task.title),
        )
    }

    fn last_reply(&mut self, job_id: &str) -> Option<String> {
        let job = self.job(job_id)?;
        let transcript = rebon_session_host::background_job_transcript_path_in(
            &rebon_harness::projects_root(),
            job,
        )?;
        rebon_session::last_turn_assistant_text(&transcript)
            .ok()
            .flatten()
    }

    fn deliver(&mut self, job_id: Option<&str>, text: &str) -> Result<(), String> {
        let target = match job_id {
            Some(job_id) => job_id.to_string(),
            None => crate::router::current_router(self.store)
                .map(|router| router.identity.job_id)
                .ok_or("no router is running to take it")?,
        };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!(
            "u-internal-scheduled-event-{}-{}",
            rebon_session_host::now_ms(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let text = rebon_tool::cron::mark_scheduled_prompt(text);
        rebon_session_host::append_background_scheduled_prompt(self.store, &target, id, text)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }

    /// The task's brief goes in as the worker's next turn, the way a
    /// rework does. Its id names the task and the attempt, so a pass that
    /// runs again before the ledger is saved files the same prompt.
    fn continue_worker(&mut self, task: &Task, job_id: &str) -> Result<(), String> {
        let id = format!(
            "u-internal-scheduled-task-{}-{}",
            task.id,
            task.attempts + 1
        );
        let text = rebon_tool::cron::mark_scheduled_prompt(&rebon_work::next_task_brief(task));
        rebon_session_host::append_background_scheduled_prompt(self.store, job_id, id, text)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }

    fn worker_fit(&mut self, job_id: &str) -> rebon_work::WorkerFit {
        match self.job(job_id) {
            Some(job) => crate::session::worker_control::worker_fit(
                &rebon_harness::projects_root(),
                job,
                rebon_session_host::now_ms(),
            ),
            None => rebon_work::WorkerFit::Unknown,
        }
    }

    fn rework(&mut self, task: &Task, round: u32, findings: &str) -> Result<(), String> {
        let worker = task
            .worker_job_id
            .as_deref()
            .ok_or("the task has no worker to send the work back to")?;
        let id = format!("u-internal-scheduled-rework-{}-{round}", task.id);
        let text =
            rebon_tool::cron::mark_scheduled_prompt(&rebon_work::rework_message(round, findings));
        rebon_session_host::append_background_scheduled_prompt(self.store, worker, id, text)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_work::{NewTask, WorkRequest};

    #[test]
    fn a_planned_task_whose_router_is_gone_is_stuck_and_the_router_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        rebon_work::file_request(
            dir.path(),
            &WorkRequest::Plan {
                router_job_id: Some("gone-router".into()),
                tasks: vec![NewTask {
                    id: "t-1".into(),
                    title: "Fix it".into(),
                    project: dir.path().display().to_string(),
                    goal: "fix it".into(),
                    done_when: "fixed".into(),
                    depends_on: Vec::new(),
                    isolate: false,
                    model_profile: None,
                    worker: None,
                }],
                requested_at_ms: 1,
            },
        )
        .unwrap();
        let host = WorkHost::new(dir.path().to_path_buf());
        let (resident, notices) = host.tick(&store, &[], 10, 3);
        assert!(!resident, "nothing left open");
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].text.contains("gone-router is gone"),
            "{:?}",
            notices
        );
        assert_eq!(notices[0].title, "Fix it");
        let saved = Ledger::load(&rebon_work::ledger_path(dir.path())).unwrap();
        assert_eq!(saved.tasks["t-1"].status, rebon_work::TaskStatus::Stuck);
    }

    #[test]
    fn a_worktree_is_frozen_once_and_an_unchanged_one_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(status.status.success(), "{args:?}: {status:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        let first = freeze_worktree(repo, "t-1: first").unwrap();
        assert!(first.is_some());
        assert_eq!(freeze_worktree(repo, "t-1: again").unwrap(), None);
        std::fs::write(repo.join("a.txt"), "two").unwrap();
        let second = freeze_worktree(repo, "t-2: second").unwrap();
        assert!(second.is_some() && second != first);
    }

    #[test]
    fn a_crashed_worker_is_said_to_be_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "p".into(),
                PathBuf::from("."),
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
        job.process.status = BackgroundJobStatus::Failed;
        job.outcome.error = Some(STALE_PID_EXIT_ERROR.to_string());
        let WorkerPhase::Failed(error) = worker_view(&job).phase else {
            panic!("failed");
        };
        assert!(error.contains("brought back automatically"), "{error}");
    }
}
