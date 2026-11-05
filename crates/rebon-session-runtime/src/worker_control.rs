//! The router's workers, as the session host keeps them: the
//! [`rebon_tool::WorkerControl`] a router session's tools use.
//!
//! A worker is a background job whose parent is the router's job. It runs in
//! auto permission mode, and its turns are marked as scheduled, not typed by
//! the person. That marking does two things: the auto-mode classifier treats
//! the brief as an agent's words rather than the person's intent, and the
//! limits on automatic turns (`rebon_session_host::autonomy`) apply to it. A
//! router may steer only its own workers; it may look up any job.
//!
//! Threads are handed over through `rebon_automation`'s request files, since
//! the supervisor is the only writer of the thread table. Tasks are planned
//! and cancelled the same way, through `rebon_work`'s: every worker works on
//! a task in the router's ledger, which the supervisor dispatches and
//! follows. [`launch_worker`] is shared with the supervisor, which starts the
//! workers of planned tasks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rebon_core::auto_mode_classifier::grant_verifier::{
    GrantVerdict, GrantVerifier, GrantVerifierRequest,
};
use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundLaunchOptions, BackgroundRuntimeFields,
    BackgroundStore,
};
use rebon_tool::{
    DispatchedWorker, GrantDecision, GrantDraft, GrantListing, PlannedTask, WorkerControl,
    WorkerDispatch,
};

/// Where `rebon router start` records the router's job, and the watchdog's.
pub const ROUTER_RECORD_FILE: &str = "router.json";
pub const WATCHDOG_RECORD_FILE: &str = "watchdog.json";

/// The job a record file under the store names.
pub fn recorded_job(store_root: &Path, file: &str) -> Option<String> {
    let bytes = std::fs::read(store_root.join(file)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("jobId")?.as_str().map(str::to_string)
}

/// The permission mode every worker runs in.
const WORKER_PERMISSION_MODE: &str = "auto";
/// How much of a worker's last reply a recall shows. The rest stays in its
/// transcript.
const RECALL_REPLY_CHARS: usize = 3_000;
const RECALL_MAX_WORKERS: usize = 30;

pub struct HostWorkerControl {
    store: BackgroundStore,
    projects_root: PathBuf,
    /// Checks a router's grant drafts against the person's own words.
    /// Without one, no grant is issued.
    grant_verifier: Option<Arc<dyn GrantVerifier>>,
}

impl HostWorkerControl {
    pub fn new(store: BackgroundStore, projects_root: PathBuf) -> Self {
        Self {
            store,
            projects_root,
            grant_verifier: None,
        }
    }

    pub fn with_grant_verifier(mut self, verifier: Arc<dyn GrantVerifier>) -> Self {
        self.grant_verifier = Some(verifier);
        self
    }

    /// The project every one of `task_ids` is in, when each is a task of
    /// `router_job_id`'s, planned or already in the ledger.
    fn grant_project(&self, router_job_id: &str, task_ids: &[String]) -> Result<String, String> {
        if task_ids.is_empty() {
            return Err("it names no task".to_string());
        }
        let mut project: Option<String> = None;
        for task_id in task_ids {
            let (router, task_project) = rebon_work::find_planned_task(self.config_home(), task_id)
                .ok_or_else(|| format!("{task_id} is not a task of yours"))?;
            if router.as_deref() != Some(router_job_id) {
                return Err(format!("{task_id} is not a task of yours"));
            }
            match &project {
                None => project = Some(task_project),
                Some(first) if first == &task_project => {}
                Some(_) => {
                    return Err(
                        "its tasks are in different projects; ask once per project".to_string()
                    )
                }
            }
        }
        Ok(project.expect("at least one task"))
    }

    /// The automation files live beside the jobs, as the supervisor reads
    /// them.
    fn config_home(&self) -> &Path {
        self.store.root()
    }

    /// The router job whose session is `session_id`. The watchdog's session
    /// acts for the router it watches: the one recorded in `router.json`.
    fn router(&self, session_id: &str) -> Result<BackgroundJobState, String> {
        let job = self
            .store
            .list_jobs()
            .map_err(|error| format!("{error:#}"))?
            .into_iter()
            .find(|job| job.identity.session_id.as_deref() == Some(session_id))
            .ok_or_else(|| {
                "this session is not a background router job, so it has no workers".to_string()
            })?;
        if recorded_job(self.store.root(), WATCHDOG_RECORD_FILE).as_deref()
            != Some(job.identity.job_id.as_str())
        {
            return Ok(job);
        }
        let router = recorded_job(self.store.root(), ROUTER_RECORD_FILE)
            .ok_or("no router is running, so there are no workers to watch")?;
        self.store
            .read_state(&router)
            .map_err(|_| format!("the router {router} is gone"))
    }

    /// One of `router`'s workers.
    fn worker(
        &self,
        router: &BackgroundJobState,
        worker_id: &str,
    ) -> Result<BackgroundJobState, String> {
        let worker = self
            .store
            .read_state(worker_id)
            .map_err(|_| format!("there is no worker {worker_id}"))?;
        if worker.identity.parent_job_id.as_deref() != Some(router.identity.job_id.as_str()) {
            return Err(format!("{worker_id} is not one of your workers"));
        }
        Ok(worker)
    }

    /// A worker a task can be given to: one of the router's, not stopped.
    fn worker_to_continue(
        &self,
        router: &BackgroundJobState,
        worker_id: &str,
    ) -> Result<BackgroundJobState, String> {
        let worker = self.worker(router, worker_id)?;
        if worker.process.status == BackgroundJobStatus::Stopped {
            return Err(format!(
                "{worker_id} was stopped; leave `worker` out to start a new one"
            ));
        }
        Ok(worker)
    }

    fn ledger(&self) -> rebon_work::Ledger {
        rebon_work::Ledger::load(&rebon_work::ledger_path(self.config_home())).unwrap_or_default()
    }

    /// File a request, and make sure a supervisor is up to take it.
    fn file(&self, request: &rebon_work::WorkRequest) -> Result<(), String> {
        rebon_work::file_request(self.config_home(), request)
            .map_err(|error| format!("{error:#}"))?;
        rebon_session_host::ensure_supervisor_running(&self.store, &crate::host::rebon_exe())
            .map_err(|error| format!("{error:#}"))
    }

    fn plan(
        &self,
        router: &BackgroundJobState,
        tasks: Vec<PlannedTask>,
    ) -> Result<Vec<String>, String> {
        if tasks.is_empty() {
            return Err("a plan needs at least one task".to_string());
        }
        let now = rebon_session_host::now_ms();
        let ids: std::collections::BTreeMap<String, String> = tasks
            .iter()
            .map(|task| (task.key.clone(), rebon_work::new_task_id(now)))
            .collect();
        if ids.len() != tasks.len() {
            return Err("each task in a plan needs its own key".to_string());
        }
        let ledger = self.ledger();
        let mut planned = Vec::new();
        for task in &tasks {
            check_project(&task.project)?;
            let mut depends_on = Vec::new();
            for dep in &task.depends_on {
                match ids.get(dep) {
                    Some(id) => depends_on.push(id.clone()),
                    None if ledger.tasks.contains_key(dep) => depends_on.push(dep.clone()),
                    None => {
                        return Err(format!(
                            "task `{}` depends on `{dep}`, which is neither a key in this plan nor a task id",
                            task.key
                        ))
                    }
                }
            }
            if let Some(worker) = task.worker.as_deref() {
                self.worker_to_continue(router, worker)?;
            }
            planned.push(rebon_work::NewTask {
                id: ids[&task.key].clone(),
                title: task.title.clone(),
                project: task.project.clone(),
                goal: task.goal.clone(),
                done_when: task.done_when.clone(),
                depends_on,
                isolate: task.isolate,
                model_profile: task.model_profile.clone(),
                worker: task.worker.clone(),
            });
        }
        let order = planned.iter().map(|task| task.id.clone()).collect();
        self.file(&rebon_work::WorkRequest::Plan {
            router_job_id: Some(router.identity.job_id.clone()),
            tasks: planned,
            requested_at_ms: now,
        })?;
        Ok(order)
    }

    fn queue_turn(&self, worker: &BackgroundJobState, message: &str) -> Result<(), String> {
        if worker.process.status == BackgroundJobStatus::Stopped {
            return Err(format!(
                "{} was stopped; dispatch a new worker instead",
                worker.identity.job_id
            ));
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = format!(
            "u-internal-scheduled-router-{}-{}",
            rebon_session_host::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let text =
            rebon_tool::cron::mark_scheduled_prompt(&format!("[From the router]\n{message}"));
        rebon_session_host::append_background_scheduled_prompt(
            &self.store,
            &worker.identity.job_id,
            id,
            text,
        )
        .map_err(|error| format!("{error:#}"))?;
        // A parked worker needs the supervisor to start it again.
        rebon_session_host::ensure_supervisor_running(&self.store, &crate::host::rebon_exe())
            .map_err(|error| format!("{error:#}"))
    }

    fn threads(&self) -> std::collections::BTreeMap<String, rebon_automation::threads::PrThread> {
        std::fs::read(rebon_automation::state_path(self.config_home()))
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<rebon_automation::AutomationState>(&bytes).ok()
            })
            .map(|state| state.threads)
            .unwrap_or_default()
    }

    fn render_task(&self, router: &BackgroundJobState, task: &rebon_work::Task) -> String {
        let mut lines = vec![format!("Task {} — {}", task.id, task.title)];
        let mut status = format!("status: {}", task.status.as_str());
        if let Some(note) = task.note.as_deref() {
            status.push_str(&format!(" ({note})"));
        }
        lines.push(status);
        lines.push(format!("project: {}", task.project));
        if !task.done_when.is_empty() {
            lines.push(format!("done when: {}", task.done_when));
        }
        if !task.depends_on.is_empty() {
            lines.push(format!("depends on: {}", task.depends_on.join(", ")));
        }
        if let Some(worktree) = task.worktree.as_deref() {
            let from = task
                .inherited_from
                .as_deref()
                .map(|dep| format!(", carried on from {dep}"))
                .unwrap_or_default();
            let commit = task
                .worktree_commit
                .as_deref()
                .map(|commit| format!(", committed as {commit}"))
                .unwrap_or_default();
            lines.push(format!("worktree: {worktree}{from}{commit}"));
        }
        if let Some(verdict) = task.verdicts.last() {
            lines.push(format!(
                "last acceptance check: {} — {} (round {} of rework)",
                if verdict.passed { "PASS" } else { "FAIL" },
                verdict.reason,
                task.rework_rounds
            ));
        }
        if let Some(worker) = task.worker_job_id.as_deref() {
            lines.push(format!(
                "worker: {worker} (Recall it for its last reply), attempts: {}",
                task.attempts
            ));
        }
        if task.router_job_id.as_deref() != Some(router.identity.job_id.as_str()) {
            lines.push("not one of your tasks".to_string());
        }
        let recent: Vec<String> = task
            .history
            .iter()
            .rev()
            .take(5)
            .map(|event| format!("  {}", event.what))
            .collect();
        if !recent.is_empty() {
            lines.push("recent:".to_string());
            lines.extend(recent);
        }
        lines.push(format!(
            "goal:\n{}",
            tail(task.goal.trim(), RECALL_REPLY_CHARS)
        ));
        lines.join("\n")
    }

    /// The router's tasks that are not settled, for the listing.
    fn render_open_tasks(&self, router: &BackgroundJobState) -> Option<String> {
        let ledger = self.ledger();
        let open: Vec<&rebon_work::Task> = ledger
            .tasks
            .values()
            .filter(|task| {
                task.router_job_id.as_deref() == Some(router.identity.job_id.as_str())
                    && !matches!(
                        task.status,
                        rebon_work::TaskStatus::Done | rebon_work::TaskStatus::Cancelled
                    )
            })
            .collect();
        if open.is_empty() {
            return None;
        }
        let mut lines = vec![format!("{} open task(s):", open.len())];
        for task in open {
            let worker = task
                .worker_job_id
                .as_deref()
                .map(|worker| format!(", worker {worker}"))
                .unwrap_or_default();
            let note = task
                .note
                .as_deref()
                .map(|note| format!(" — {note}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {} [{}] {}{worker}{note}",
                task.id,
                task.status.as_str(),
                task.title
            ));
        }
        Some(lines.join("\n"))
    }

    fn render_workers(&self, router: &BackgroundJobState) -> Result<String, String> {
        let mut workers: Vec<_> = self
            .store
            .list_jobs()
            .map_err(|error| format!("{error:#}"))?
            .into_iter()
            .filter(|job| {
                job.identity.parent_job_id.as_deref() == Some(router.identity.job_id.as_str())
            })
            .collect();
        let tasks = self.render_open_tasks(router);
        if workers.is_empty() {
            return Ok(match tasks {
                Some(tasks) => format!("{tasks}\nNo worker has started yet."),
                None => "You have no workers and no open tasks.".to_string(),
            });
        }
        workers.sort_by_key(|job| std::cmp::Reverse(job.process.updated_at_ms));
        let total = workers.len();
        let now = rebon_session_host::now_ms();
        let mut lines = vec![format!("{total} worker(s), most recent first:")];
        for job in workers.iter().take(RECALL_MAX_WORKERS) {
            let summary = job
                .outcome
                .summary
                .as_deref()
                .and_then(|summary| summary.lines().next())
                .map(|line| format!(" — {line}"))
                .unwrap_or_default();
            let session = self
                .finished_session_note(job, now)
                .map(|note| format!(" · {note}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {} [{}] {} ({}){session}{summary}",
                job.identity.job_id,
                status_name(job.process.status),
                job.identity.name,
                job.identity.cwd
            ));
        }
        if total > RECALL_MAX_WORKERS {
            lines.push(format!("… and {} older", total - RECALL_MAX_WORKERS));
        }
        if let Some(tasks) = tasks {
            lines.insert(0, tasks);
        }
        Ok(lines.join("\n"))
    }

    /// [`session_note`] for a worker whose turn has ended; nothing for one
    /// that is busy, stopped or failed, which takes no task either way.
    fn finished_session_note(&self, job: &BackgroundJobState, now_ms: u64) -> Option<String> {
        if !matches!(
            job.process.status,
            BackgroundJobStatus::Idle | BackgroundJobStatus::Succeeded
        ) {
            return None;
        }
        session_note(&worker_activity(&self.projects_root, job)?, now_ms)
    }

    fn render_thread(&self, key: &str) -> String {
        let Some(thread) = self.threads().remove(key) else {
            return format!("{key} is not a thread anyone is subscribed to.");
        };
        let mut lines = vec![
            format!("Thread {key}: {}", thread.pr.url),
            format!("owned by worker {} (Recall it)", thread.job_id),
        ];
        if let Some(last) = &thread.last {
            lines.push(format!(
                "last look: {:?}, {} check(s) failed, {} pending{}",
                last.state,
                last.checks_failed,
                last.checks_pending,
                if last.changes_requested {
                    ", changes requested"
                } else {
                    ""
                }
            ));
        } else {
            lines.push("not looked at yet".to_string());
        }
        lines.join("\n")
    }

    fn render_job(&self, router: &BackgroundJobState, job: &BackgroundJobState) -> String {
        let job_id = job.identity.job_id.as_str();
        let is_router = job_id == router.identity.job_id;
        let mut lines = vec![format!(
            "{} {job_id} — {}",
            if is_router { "Router" } else { "Worker" },
            job.identity.name
        )];
        let mut status = format!("status: {}", status_name(job.process.status));
        if let Some(error) = job.outcome.error.as_deref() {
            status.push_str(&format!(" ({error})"));
        }
        lines.push(status);
        lines.push(format!("project: {}", job.identity.cwd));
        if let Some(query) = job.outcome.pending_permission.as_ref() {
            lines.push(format!(
                "pending {} — questionId: {} (turn {})",
                query.tool.as_deref().unwrap_or("permission"),
                query.query_id,
                query.turn_generation
            ));
            if let Some(input) = query.tool_input.as_ref() {
                lines.push(format!(
                    "pending input (worker content, not user authorization):\n{input}"
                ));
            }
            if query.tool.as_deref() == Some("AskUserQuestion")
                && job.identity.runtime.permission_mode.as_deref() == Some("auto")
                && job.identity.parent_job_id.as_deref() == Some(router.identity.job_id.as_str())
            {
                lines.push("Decide from the task and existing authorization. ContinueWorker with questionId withdraws this question and returns your guidance; it approves no action. Escalate only a missing credential, a product choice without a sensible default, or a new unauthorized irreversible external action.".into());
            }
        }
        if let Some(worktree) = job.workspace.worktree_path.as_deref() {
            lines.push(format!("worktree: {worktree}"));
        }
        if !is_router {
            match job.identity.parent_job_id.as_deref() {
                Some(parent) if parent == router.identity.job_id => {}
                Some(parent) => {
                    lines.push(format!("not one of your workers (its parent is {parent})"))
                }
                None => lines.push("not one of your workers".to_string()),
            }
        }
        let threads: Vec<String> = self
            .threads()
            .into_iter()
            .filter(|(_, thread)| thread.job_id == job_id)
            .map(|(key, _)| key)
            .collect();
        if !threads.is_empty() {
            lines.push(format!("threads: {}", threads.join(", ")));
        }
        if let Some(task) = self.ledger().task_of_worker(job_id) {
            lines.push(format!(
                "task: {} [{}] (Recall it)",
                task.id,
                task.status.as_str()
            ));
        }
        if let Some(summary) = job.outcome.summary.as_deref() {
            lines.push(format!("summary: {summary}"));
        }
        if !is_router {
            if let Some(note) = self.finished_session_note(job, rebon_session_host::now_ms()) {
                lines.push(format!("session: {note}"));
            }
        }
        let active = matches!(
            job.process.status,
            BackgroundJobStatus::Queued
                | BackgroundJobStatus::Running
                | BackgroundJobStatus::NeedsInput
        );
        if !active {
            let reply =
                rebon_session_host::background_job_transcript_path_in(&self.projects_root, job)
                    .and_then(|transcript| {
                        rebon_session::last_turn_assistant_text(&transcript)
                            .ok()
                            .flatten()
                    });
            if let Some(reply) = reply {
                lines.push(format!(
                    "last reply:\n{}",
                    tail(reply.trim(), RECALL_REPLY_CHARS)
                ));
            }
        }
        lines.join("\n")
    }

    fn recall_sync(&self, router_session_id: &str, id: Option<&str>) -> Result<String, String> {
        let router = self.router(router_session_id)?;
        let Some(id) = id else {
            return self.render_workers(&router);
        };
        if id == "router" {
            return Ok(self.render_job(&router, &router));
        }
        if let Some(pr) = rebon_automation::threads::parse_thread_ref(id) {
            return Ok(self.render_thread(&pr.key()));
        }
        if let Some(key) = rebon_work::normalize_thread_key(id) {
            return Ok(match self.ledger().subscriptions.get(&key) {
                Some(owner) => format!("Thread {key}: owned by {owner} (Recall it)"),
                None => format!("{key} is a thread nobody owns; its events come to you."),
            });
        }
        if let Some(task) = self.ledger().tasks.get(id) {
            return Ok(self.render_task(&router, task));
        }
        let job = self
            .store
            .read_state(id)
            .map_err(|_| format!("nothing is known by the id {id}"))?;
        Ok(self.render_job(&router, &job))
    }
}

#[async_trait]
impl WorkerControl for HostWorkerControl {
    async fn plan_tasks(
        &self,
        router_session_id: &str,
        tasks: Vec<PlannedTask>,
    ) -> Result<Vec<String>, String> {
        let router = self.router(router_session_id)?;
        self.plan(&router, tasks)
    }

    async fn cancel_task(
        &self,
        router_session_id: &str,
        task_id: &str,
        reason: Option<&str>,
    ) -> Result<(), String> {
        let router = self.router(router_session_id)?;
        let ledger = self.ledger();
        let task = ledger
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("there is no task {task_id}"))?;
        if task.router_job_id.as_deref() != Some(router.identity.job_id.as_str()) {
            return Err(format!("{task_id} is not one of your tasks"));
        }
        if let Some(worker_id) = task.worker_job_id.as_deref() {
            if let Ok(mut worker) = self.worker(&router, worker_id) {
                rebon_session_host::stop_background_job_tree_in_store(&self.store, &mut worker)
                    .map_err(|error| format!("{error:#}"))?;
            }
        }
        self.file(&rebon_work::WorkRequest::Cancel {
            task_id: task_id.to_string(),
            reason: reason.map(str::to_string),
            requested_at_ms: rebon_session_host::now_ms(),
        })
    }

    async fn dispatch(
        &self,
        router_session_id: &str,
        request: WorkerDispatch,
    ) -> Result<DispatchedWorker, String> {
        let router = self.router(router_session_id)?;
        // A worker started now is still a task: the supervisor follows it
        // from the ledger like any planned one.
        let task_id = rebon_work::new_task_id(rebon_session_host::now_ms());
        let task = rebon_work::NewTask {
            id: task_id.clone(),
            title: request.title.clone(),
            project: request.project.clone(),
            goal: request.brief.clone(),
            done_when: request.done_when.clone(),
            depends_on: Vec::new(),
            isolate: request.isolate,
            model_profile: request.model_profile.clone(),
            worker: request.worker.clone(),
        };
        // A task for an existing worker is the supervisor's to hand over:
        // it waits while the worker is busy, and starts a fresh one instead
        // if the worker's context has grown long.
        if let Some(worker_id) = request.worker.as_deref() {
            check_project(&request.project)?;
            let worker = self.worker_to_continue(&router, worker_id)?;
            self.file(&rebon_work::WorkRequest::Plan {
                router_job_id: Some(router.identity.job_id.clone()),
                tasks: vec![task],
                requested_at_ms: rebon_session_host::now_ms(),
            })?;
            let busy = matches!(
                worker.process.status,
                BackgroundJobStatus::Queued
                    | BackgroundJobStatus::Running
                    | BackgroundJobStatus::NeedsInput
            );
            return Ok(DispatchedWorker {
                worker_id: worker_id.to_string(),
                task_id: Some(task_id),
                note: Some(if busy {
                    format!(
                        "{worker_id} is busy; it takes the task as its next turn once it is free"
                    )
                } else {
                    format!("{worker_id} takes the task as its next turn")
                }),
            });
        }
        let mut dispatched = launch_worker(&self.store, &router, &request, Some(&task_id))?;
        self.file(&rebon_work::WorkRequest::Bind {
            router_job_id: Some(router.identity.job_id.clone()),
            task,
            worker_job_id: dispatched.worker_id.clone(),
            requested_at_ms: rebon_session_host::now_ms(),
        })?;
        dispatched.task_id = Some(task_id);
        Ok(dispatched)
    }

    async fn continue_worker(
        &self,
        router_session_id: &str,
        worker_id: &str,
        message: &str,
        question_id: Option<u64>,
    ) -> Result<(), String> {
        let router = self.router(router_session_id)?;
        let worker = self.worker(&router, worker_id)?;
        if let Some(question_id) = question_id {
            let request = router_question_reply(&worker, question_id, message)?;
            let pending = worker
                .outcome
                .pending_permission
                .as_ref()
                .expect("validated question");
            let endpoint = pending.endpoint.as_ref().ok_or(
                "the question has no live endpoint; Recall again after the worker resumes",
            )?;
            let owner = rebon_session_host::OwnerHandle::for_worker(
                worker.identity.session_id.as_deref().unwrap_or_default(),
                Some(&worker.identity.job_id),
                endpoint,
            );
            return rebon_session_host::SessionHostConnection::new(owner)
                .call(request)
                .map(|_| ())
                .map_err(|error| {
                    format!(
                        "router question reply was not applied: {error}; Recall before retrying"
                    )
                });
        }
        if worker.identity.runtime.permission_mode.as_deref() == Some("auto")
            && worker
                .outcome
                .pending_permission
                .as_ref()
                .is_some_and(|query| query.tool.as_deref() == Some("AskUserQuestion"))
        {
            return Err("the worker is blocked on a question, not a turn boundary; Recall it and pass questionId with your decision".into());
        }
        self.queue_turn(&worker, message)
    }

    async fn stop_worker(&self, router_session_id: &str, worker_id: &str) -> Result<(), String> {
        let router = self.router(router_session_id)?;
        let mut worker = self.worker(&router, worker_id)?;
        rebon_session_host::stop_background_job_tree_in_store(&self.store, &mut worker)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }

    async fn subscribe_thread(
        &self,
        router_session_id: &str,
        thread: &str,
        worker_id: &str,
    ) -> Result<String, String> {
        let router = self.router(router_session_id)?;
        let owner = if worker_id == router.identity.job_id {
            router
        } else {
            self.worker(&router, worker_id)?
        };
        // Any other thread is the task ledger's: a chat, a mail, anything a
        // source names with `scheme:rest`.
        let Some(pr) = rebon_automation::threads::parse_thread_ref(thread) else {
            let key = rebon_work::normalize_thread_key(thread).ok_or_else(|| {
                format!(
                    "{thread} is not a thread: write pr:owner/repo#12, a pull request's URL, or scheme:name such as im:team#42"
                )
            })?;
            self.file(&rebon_work::WorkRequest::Subscribe {
                thread: key.clone(),
                job_id: owner.identity.job_id,
                requested_at_ms: rebon_session_host::now_ms(),
            })?;
            return Ok(key);
        };
        // `gh` runs where the owner works: its worktree, else its project.
        let cwd = owner
            .workspace
            .worktree_path
            .clone()
            .unwrap_or_else(|| owner.identity.cwd.clone());
        let key = pr.key();
        rebon_automation::threads::request_subscription(
            self.config_home(),
            &rebon_automation::threads::SubscriptionRequest {
                pr,
                job_id: owner.identity.job_id,
                cwd,
                requested_at_ms: rebon_session_host::now_ms(),
            },
        )
        .map_err(|error| format!("{error:#}"))?;
        Ok(key)
    }

    async fn recall(&self, router_session_id: &str, id: Option<&str>) -> Result<String, String> {
        self.recall_sync(router_session_id, id)
    }

    async fn grant_authorizations(
        &self,
        router_session_id: &str,
        drafts: Vec<GrantDraft>,
    ) -> Result<Vec<GrantDecision>, String> {
        let router = self.router(router_session_id)?;
        // The watchdog acts for the router, but the person's words were heard
        // by the router's own process: only the router can ask.
        if router.identity.session_id.as_deref() != Some(router_session_id) {
            return Err("only the router itself can ask for authorizations".to_string());
        }
        let verifier = self
            .grant_verifier
            .clone()
            .ok_or("this host cannot check authorizations, so it issues none")?;
        let router_job_id = router.identity.job_id.clone();
        let router_pid = router.process.pid.ok_or("router has no live process")?;
        let router_pid_identity = router
            .process
            .pid_identity
            .clone()
            .ok_or("router has no verifiable process identity")?;
        let statements = crate::host::user_grants::statements(&router_job_id);
        let statements: Vec<_> = statements.last().cloned().into_iter().collect();
        let source_epoch = rebon_work::load_grants(self.config_home()).user_epoch(&router_job_id);
        let mut decisions = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let refuse = |reason: String| GrantDecision {
                action: draft.action.clone(),
                target: draft.target.clone(),
                grant_id: None,
                reason,
            };
            let project = match self.grant_project(&router_job_id, &draft.task_ids) {
                Ok(project) => project,
                Err(reason) => {
                    decisions.push(refuse(reason));
                    continue;
                }
            };
            let verdict = verifier
                .verify(&GrantVerifierRequest {
                    action: draft.action.clone(),
                    target: draft.target.clone(),
                    summary: draft.summary.clone(),
                    statements: statements.clone(),
                })
                .await;
            let (user_quote, proposal_quote) = match verdict {
                GrantVerdict::Granted {
                    user_quote,
                    proposal_quote,
                } => (user_quote, proposal_quote),
                GrantVerdict::Refused { reason } => {
                    decisions.push(refuse(format!("not authorized: {reason}")));
                    continue;
                }
            };
            let now = rebon_session_host::now_ms();
            let grant = rebon_work::Grant {
                id: rebon_work::new_grant_id(now),
                router_job_id: router_job_id.clone(),
                router_pid,
                router_pid_identity: router_pid_identity.clone(),
                project,
                task_ids: draft.task_ids.clone(),
                action: draft.action.clone(),
                target: draft.target.clone(),
                summary: draft.summary.clone(),
                user_quote: user_quote.clone(),
                proposal_quote,
                issued_at_ms: now,
                expires_at_ms: now + rebon_work::MAX_GRANT_AGE_MS,
                revoked_at_ms: None,
                consent_withdrawn: false,
            };
            let grant_id = grant.id.clone();
            let ledger = self.ledger();
            match rebon_work::update_grants(self.config_home(), &ledger, now, |book| {
                if book.user_epoch(&router_job_id) != source_epoch
                    || book.denied_by_later_message(&router_job_id, &draft.action, &draft.target)
                {
                    false
                } else {
                    book.grants.push(grant);
                    true
                }
            }) {
                Ok(false) => decisions.push(refuse(
                    "a later user message withdrew this step or changed the authorization context"
                        .into(),
                )),
                Ok(true) => {
                    let _ = self.store.append_event(
                        &router_job_id,
                        "authorization_granted",
                        serde_json::json!({
                            "grantId": &grant_id,
                            "action": &draft.action,
                            "target": &draft.target,
                            "taskIds": &draft.task_ids,
                            "userQuote": &user_quote,
                        }),
                    );
                    decisions.push(GrantDecision {
                        action: draft.action.clone(),
                        target: draft.target.clone(),
                        grant_id: Some(grant_id),
                        reason: format!("the person said: \u{201c}{user_quote}\u{201d}"),
                    });
                }
                Err(error) => decisions.push(refuse(format!("could not be recorded: {error:#}"))),
            }
        }
        Ok(decisions)
    }

    async fn revoke_authorizations(
        &self,
        router_session_id: &str,
        ids: Vec<String>,
    ) -> Result<Vec<String>, String> {
        let router = self.router(router_session_id)?;
        let ledger = self.ledger();
        let now = rebon_session_host::now_ms();
        let revoked = rebon_work::update_grants(self.config_home(), &ledger, now, |book| {
            book.revoke(Some(&router.identity.job_id), &ids, &ledger, now)
        })
        .map_err(|error| format!("{error:#}"))?;
        if !revoked.is_empty() {
            let _ = self.store.append_event(
                &router.identity.job_id,
                "authorization_revoked",
                serde_json::json!({ "grantIds": revoked }),
            );
        }
        Ok(revoked)
    }

    async fn list_authorizations(
        &self,
        router_session_id: &str,
    ) -> Result<Vec<GrantListing>, String> {
        let router = self.router(router_session_id)?;
        let ledger = self.ledger();
        let now = rebon_session_host::now_ms();
        let mut grants: Vec<rebon_work::Grant> = rebon_work::load_grants(self.config_home())
            .grants
            .into_iter()
            .filter(|grant| grant.router_job_id == router.identity.job_id)
            .collect();
        grants.sort_by(|a, b| b.issued_at_ms.cmp(&a.issued_at_ms));
        Ok(grants
            .into_iter()
            .map(|grant| GrantListing {
                state: grant.state(&ledger, now).as_str().to_string(),
                grant_id: grant.id,
                summary: grant.summary,
                task_ids: grant.task_ids,
            })
            .collect())
    }
}

/// Withdraw a router-owned auto worker's question, never approve a tool.
/// Both ids are fenced by the existing permission-answer IPC transaction.
fn router_question_reply(
    worker: &BackgroundJobState,
    question_id: u64,
    message: &str,
) -> Result<rebon_session_host::BackgroundIpcRequest, String> {
    let pending = worker
        .outcome
        .pending_permission
        .as_ref()
        .ok_or("the worker has no pending question; Recall before retrying")?;
    if worker.identity.parent_job_id.is_none()
        || worker.identity.runtime.permission_mode.as_deref() != Some("auto")
        || worker.process.status != BackgroundJobStatus::NeedsInput
        || pending.tool.as_deref() != Some("AskUserQuestion")
    {
        return Err("router guidance can withdraw only an auto worker's question, never grant a tool permission".into());
    }
    if pending.query_id != question_id || pending.turn_generation != worker.process.turn_generation
    {
        return Err("the pending question changed; Recall before retrying".into());
    }
    if message.trim().is_empty() {
        return Err("a router decision must not be empty".into());
    }
    let reject = pending
        .options
        .iter()
        .find(|option| {
            option.option_id == "reject_once"
                && rebon_session_host::parse_permission_option_kind(&option.kind)
                    == Some(rebon_core::permission::PermissionOptionKind::RejectOnce)
        })
        .ok_or("the question has no reject-once option; no decision was sent")?;
    Ok(rebon_session_host::BackgroundIpcRequest::PermissionAnswer {
        query_id: question_id,
        turn_generation: pending.turn_generation,
        option_id: Some(reject.option_id.clone()),
        extra_text: Some(format!("{}\nRouter decision: {}\nContinue within the task's existing authorization. This grants no tool permissions and does not override permission denials.",
            rebon_tool::worker_control::ROUTER_QUESTION_GUIDANCE_PREFIX, message.trim())),
        updated_input: None,
    })
}

fn check_project(project: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(project);
    if !path.is_absolute() || !path.is_dir() {
        return Err(format!(
            "{project} is not a project directory; give its absolute path"
        ));
    }
    Ok(path)
}

/// Start a worker under `router` on `request`, for the task `task_id` when
/// it has one. Used by `DispatchWorker` and by the supervisor for a planned
/// task.
pub fn launch_worker(
    store: &BackgroundStore,
    router: &BackgroundJobState,
    request: &WorkerDispatch,
    task_id: Option<&str>,
) -> Result<DispatchedWorker, String> {
    let project = check_project(&request.project)?;
    let mut notes = Vec::new();
    let isolate = request.isolate && in_git_repo(&project);
    if request.isolate && !isolate {
        notes.push(
            "the project is not a git repository, so the worker works in the directory itself"
                .to_string(),
        );
    }
    let model = match request.model_profile.as_deref() {
        Some(profile) => {
            let model = crate::rebon_config::active_provider_profile_model(profile);
            if model.is_none() {
                notes.push(format!(
                    "no model is set for the `{profile}` profile, so the worker uses the default model"
                ));
            }
            model
        }
        None => None,
    };
    let job = launch_job(
        store,
        router,
        project,
        &request.title,
        &worker_brief(router, request, task_id),
        isolate,
        model,
    )?;
    Ok(DispatchedWorker {
        worker_id: job.identity.job_id,
        task_id: task_id.map(str::to_string),
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    })
}

/// Whether the finished worker `job` should take another task, by the
/// group's warmth rules (`rebon_group::warmth`): idle past its prompt cache
/// (an hour) it is cold but still takes it, compacting as it resumes;
/// carrying more than 60% of its window it is long, and a fresh worker
/// takes the task instead.
pub fn worker_fit(
    projects_root: &Path,
    job: &BackgroundJobState,
    now_ms: u64,
) -> rebon_work::WorkerFit {
    fit_of(worker_activity(projects_root, job).as_ref(), now_ms)
}

fn worker_activity(
    projects_root: &Path,
    job: &BackgroundJobState,
) -> Option<rebon_group::warmth::Activity> {
    let session_id = job.identity.session_id.as_deref()?;
    let transcript = rebon_session_host::background_job_transcript_path_in(projects_root, job)?;
    Some(rebon_group::warmth::rebon_transcript_activity(
        &transcript,
        session_id,
    ))
}

fn fit_of(activity: Option<&rebon_group::warmth::Activity>, now_ms: u64) -> rebon_work::WorkerFit {
    use rebon_group::warmth::{self, State};
    use rebon_work::WorkerFit;
    let Some(activity) = activity else {
        return WorkerFit::Unknown;
    };
    let rules = rebon_group::model::Warmth::default();
    if warmth::long_context(activity, &rules) {
        return WorkerFit::Long;
    }
    match warmth::state(activity, &rules, now_ms) {
        State::Warm => WorkerFit::Warm,
        State::Cold => WorkerFit::Cold,
        // The job says its turn ended, whatever the transcript's last row
        // says; only how long it has sat idle is left to read.
        State::Busy => {
            let idle_ms = activity
                .last_activity_ms
                .map_or(0, |last| now_ms.saturating_sub(last));
            if idle_ms > u64::from(rules.idle_minutes) * 60_000 {
                WorkerFit::Cold
            } else {
                WorkerFit::Warm
            }
        }
        State::Unknown => WorkerFit::Unknown,
    }
}

/// How a finished worker's session stands, for the router deciding where
/// the next task goes: how long it has been idle, how much context it
/// carries, and what that means for giving it more work.
fn session_note(activity: &rebon_group::warmth::Activity, now_ms: u64) -> Option<String> {
    let idle_minutes = now_ms.saturating_sub(activity.last_activity_ms?) / 60_000;
    let idle = if idle_minutes >= 120 {
        format!("{}h", idle_minutes / 60)
    } else {
        format!("{idle_minutes}m")
    };
    let context = activity
        .context_tokens
        .map(|tokens| format!(", context ~{}k tokens", (tokens + 500) / 1000))
        .unwrap_or_default();
    let fit = match fit_of(Some(activity), now_ms) {
        rebon_work::WorkerFit::Warm => "warm: a good one for follow-up work",
        rebon_work::WorkerFit::Cold => {
            "past its prompt cache: it compacts its history before it takes more work"
        }
        rebon_work::WorkerFit::Long => "its context is long: new work goes to a fresh worker",
        rebon_work::WorkerFit::Unknown => return None,
    };
    Some(format!("idle {idle}{context}; {fit}"))
}

/// Start the acceptance check of `task`, in `cwd`: its worker's worktree,
/// or the project. A check runs on the `reviewer` profile's model when one
/// is set. Used by the supervisor.
pub fn launch_verifier(
    store: &BackgroundStore,
    router: &BackgroundJobState,
    task: &rebon_work::Task,
    cwd: &str,
) -> Result<String, String> {
    let project = check_project(cwd)?;
    let model =
        crate::rebon_config::active_provider_profile_model(rebon_types::MODEL_PROFILE_REVIEWER);
    let job = launch_job(
        store,
        router,
        project,
        &format!("Check: {}", task.title),
        &rebon_work::verifier_brief(task),
        false,
        model,
    )?;
    Ok(job.identity.job_id)
}

/// A background job under `router`, in auto permission mode, its prompt
/// marked as scheduled.
fn launch_job(
    store: &BackgroundStore,
    router: &BackgroundJobState,
    project: PathBuf,
    title: &str,
    prompt: &str,
    isolate: bool,
    model: Option<String>,
) -> Result<BackgroundJobState, String> {
    let options = BackgroundLaunchOptions {
        prompt: rebon_tool::cron::mark_scheduled_prompt(prompt),
        images: Vec::new(),
        cwd: project,
        isolate_in_worktree: isolate,
        require_worktree: false,
        // Its commits are what the router hands on; keep them.
        preserve_worktree_on_success: isolate,
        queue_session: false,
        runtime: BackgroundRuntimeFields {
            provider: None,
            model,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: Some(WORKER_PERMISSION_MODE.to_string()),
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        },
        name: Some(title.to_string()),
        agent_type: None,
        parent_job_id: Some(router.identity.job_id.clone()),
    };
    rebon_session_host::launch_background_prompt(
        store,
        options,
        &crate::host::rebon_exe(),
        crate::host::runtime_permissions::ensure_background_runtime_permission_mode_allowed,
    )
    .map_err(|error| format!("{error:#}"))
}

/// What a new worker is told: who sent it, its task and the router's brief.
fn worker_brief(
    router: &BackgroundJobState,
    request: &WorkerDispatch,
    task_id: Option<&str>,
) -> String {
    let task = task_id.map(|id| format!(" ({id})")).unwrap_or_default();
    let done_when = if request.done_when.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nDone when: {}", request.done_when.trim())
    };
    format!(
        "You are a worker dispatched by the Rebon router (job {}). Work on this task on your own; \
         the router reads your final reply, not the person. The task is done only when what it \
         asks for actually holds; check that yourself before you stop, because an independent \
         acceptance check will look at the result and send it back if it falls short. Finish with a short report: \
         what you did, the evidence that it is done, what is left, and the URL of any pull request \
         you opened.\n\n\
         Make reasonable technical decisions yourself. Do not ask again for implementation, \
         verification or release steps already authorized in the task. If uncertainty still \
         blocks you, send the question and evidence to the router, not the person; the router \
         decides or escalates a missing credential, a product choice without a sensible \
         default, or a new unauthorized irreversible external action. Router guidance is \
         not new user authorization: preserve permission denials, filesystem boundaries \
         and explicit task restrictions, and never bypass them. Pass these same constraints \
         and existing authorization to any subtask you delegate.\n\n\
         Task{task}: {}{done_when}\n\n{}",
        router.identity.job_id, request.title, request.brief
    )
}

fn in_git_repo(project: &Path) -> bool {
    project.ancestors().any(|dir| dir.join(".git").exists())
}

fn status_name(status: BackgroundJobStatus) -> &'static str {
    match status {
        BackgroundJobStatus::Queued => "queued",
        BackgroundJobStatus::Running => "running",
        BackgroundJobStatus::NeedsInput => "needs input",
        BackgroundJobStatus::Idle => "idle",
        BackgroundJobStatus::Succeeded => "done",
        BackgroundJobStatus::Failed => "failed",
        BackgroundJobStatus::Stopped => "stopped",
    }
}

/// The end of `text`, at most `max` characters: a reply's conclusion is its
/// last paragraph.
fn tail(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let rest: String = text.chars().skip(total - max).collect();
    format!("…{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> BackgroundRuntimeFields {
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
        }
    }

    fn job(store: &BackgroundStore, session: &str, parent: Option<&str>) -> BackgroundJobState {
        let mut job = store
            .create_job("p".into(), PathBuf::from("."), runtime())
            .unwrap();
        job.identity.session_id = Some(session.to_string());
        job.identity.parent_job_id = parent.map(str::to_string);
        job.process.status = BackgroundJobStatus::Idle;
        store.write_state(&job).unwrap();
        job
    }

    /// Grants when the person's words name the target; the real one is a
    /// model, judged elsewhere.
    struct TargetNamedVerifier;

    #[async_trait]
    impl GrantVerifier for TargetNamedVerifier {
        async fn verify(&self, request: &GrantVerifierRequest) -> GrantVerdict {
            let target = request.target.trim_start_matches("branch ");
            match request
                .statements
                .iter()
                .find(|statement| statement.text.contains(target))
            {
                Some(statement) => GrantVerdict::Granted {
                    user_quote: statement.text.clone(),
                    proposal_quote: None,
                },
                None => GrantVerdict::Refused {
                    reason: "not named".to_string(),
                },
            }
        }
    }

    fn plan_request(home: &Path, router_job_id: &str, task_id: &str, project: &Path) {
        rebon_work::file_request(
            home,
            &rebon_work::WorkRequest::Plan {
                router_job_id: Some(router_job_id.to_string()),
                tasks: vec![rebon_work::NewTask {
                    id: task_id.to_string(),
                    title: "fix CI".into(),
                    project: project.to_string_lossy().to_string(),
                    goal: "fix CI".into(),
                    done_when: String::new(),
                    depends_on: Vec::new(),
                    isolate: false,
                    model_profile: None,
                    worker: None,
                }],
                requested_at_ms: rebon_session_host::now_ms(),
            },
        )
        .unwrap();
    }

    fn draft(target: &str, task_id: &str) -> GrantDraft {
        GrantDraft {
            action: "git push".into(),
            target: target.into(),
            summary: format!("push to {target}"),
            task_ids: vec![task_id.to_string()],
        }
    }

    #[tokio::test]
    async fn grants_rest_on_the_persons_words_and_reach_only_the_tasks_worker() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let project = dir.path().join("app");
        std::fs::create_dir_all(&project).unwrap();
        let mut router = job(&store, "router-session", None);
        router.process.pid = Some(std::process::id());
        router.process.pid_identity = rebon_session_host::process_identity(std::process::id());
        store.write_state(&router).unwrap();
        let other_router = job(&store, "other-router-session", None);
        let router_id = router.identity.job_id.clone();
        plan_request(dir.path(), &router_id, "t-mine", &project);
        plan_request(
            dir.path(),
            &other_router.identity.job_id,
            "t-theirs",
            &project,
        );
        crate::host::user_grants::note_user_prompt(
            dir.path(),
            &router_id,
            "fix CI, then push to main",
        )
        .unwrap();

        let unchecked = HostWorkerControl::new(store.clone(), dir.path().join("projects"));
        assert!(
            unchecked
                .grant_authorizations("router-session", vec![draft("branch main", "t-mine")])
                .await
                .is_err(),
            "no verifier, no grant"
        );

        let control = HostWorkerControl::new(store.clone(), dir.path().join("projects"))
            .with_grant_verifier(Arc::new(TargetNamedVerifier));
        let decisions = control
            .grant_authorizations(
                "router-session",
                vec![
                    draft("branch main", "t-mine"),
                    draft("branch release", "t-mine"),
                    draft("branch main", "t-theirs"),
                    draft("branch main", "t-nowhere"),
                ],
            )
            .await
            .unwrap();
        let issued: Vec<bool> = decisions.iter().map(|d| d.grant_id.is_some()).collect();
        assert_eq!(issued, [true, false, false, false], "{decisions:?}");
        assert!(decisions[2].reason.contains("not a task of yours"));

        // The task's worker sees it once the supervisor has bound the task.
        let mut ledger = rebon_work::Ledger::default();
        let mut task = rebon_work::Task {
            id: "t-mine".into(),
            title: "fix CI".into(),
            project: project.to_string_lossy().to_string(),
            goal: "fix CI".into(),
            done_when: String::new(),
            depends_on: Vec::new(),
            isolate: false,
            model_profile: None,
            router_job_id: Some(router_id.clone()),
            status: rebon_work::TaskStatus::Running,
            worker_job_id: Some("w-mine".into()),
            attempts: 1,
            verifier_job_id: None,
            verifier_attempts: 0,
            rework_rounds: 0,
            verdicts: Vec::new(),
            worktree: None,
            worktree_commit: None,
            inherited_from: None,
            requested_worker: None,
            handed_on_to: None,
            created_at_ms: rebon_session_host::now_ms(),
            updated_at_ms: rebon_session_host::now_ms(),
            note: None,
            history: Vec::new(),
        };
        ledger.tasks.insert(task.id.clone(), task.clone());
        ledger.save(&rebon_work::ledger_path(dir.path())).unwrap();

        use rebon_tool::UserGrantSource as _;
        let mine = crate::host::user_grants::TaskGrantSource::new(
            dir.path().to_path_buf(),
            "w-mine".into(),
        );
        let initial = mine.snapshot().await;
        assert!(!initial.authorization_withdrawn);
        let initial_revision = initial.revision.clone();
        let seen = initial.grants;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target, "branch main");
        assert_eq!(seen[0].user_quote, "fix CI, then push to main");
        let stranger =
            crate::host::user_grants::TaskGrantSource::new(dir.path().to_path_buf(), "w-x".into());
        assert!(stranger.grants().await.is_empty());

        // The live grant is visible to the user and persists until withdrawn.
        let listed = control.list_authorizations("router-session").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, "live");
        assert_eq!(mine.grants().await.len(), 1);

        // Recovery with another router process cannot inherit the prior
        // process's grant even while it remains in grants.json.
        router.process.pid_identity = Some("restarted-process".into());
        store.write_state(&router).unwrap();
        assert!(mine.grants().await.is_empty());
        router.process.pid_identity = rebon_session_host::process_identity(std::process::id());
        store.write_state(&router).unwrap();
        assert_eq!(mine.grants().await.len(), 1);
        router.process.pid = None;
        store.write_state(&router).unwrap();
        assert!(
            mine.grants().await.is_empty(),
            "a missing source fails closed"
        );
        router.process.pid = Some(std::process::id());
        store.write_state(&router).unwrap();
        assert_eq!(mine.grants().await.len(), 1);

        // A later direct denial fences every worker of the router, not just
        // the worker whose grant was active.
        crate::host::user_grants::note_user_prompt(
            dir.path(),
            &router_id,
            "actually don't push to main",
        )
        .unwrap();
        let withdrawn = mine.snapshot().await;
        assert!(withdrawn.grants.is_empty());
        assert!(withdrawn.authorization_withdrawn);
        assert_ne!(withdrawn.revision, initial_revision);
        let denied = control
            .grant_authorizations("router-session", vec![draft("branch main", "t-mine")])
            .await
            .unwrap();
        assert!(denied[0].grant_id.is_none(), "{denied:?}");

        // A withdrawn grant cannot revive after its task settles.
        task.status = rebon_work::TaskStatus::Done;
        ledger.tasks.insert(task.id.clone(), task);
        ledger.save(&rebon_work::ledger_path(dir.path())).unwrap();
        assert!(mine.grants().await.is_empty());

        crate::host::user_grants::forget(&router_id);
    }

    #[tokio::test]
    async fn nothing_typed_through_the_live_door_means_nothing_to_grant_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let project = dir.path().join("app");
        std::fs::create_dir_all(&project).unwrap();
        let mut router = job(&store, "quiet-router-session", None);
        router.process.pid = Some(std::process::id());
        router.process.pid_identity = rebon_session_host::process_identity(std::process::id());
        store.write_state(&router).unwrap();
        plan_request(dir.path(), &router.identity.job_id, "t-q", &project);
        let control = HostWorkerControl::new(store, dir.path().join("projects"))
            .with_grant_verifier(Arc::new(TargetNamedVerifier));
        let decisions = control
            .grant_authorizations("quiet-router-session", vec![draft("branch main", "t-q")])
            .await
            .unwrap();
        assert!(decisions[0].grant_id.is_none());
    }

    #[tokio::test]
    async fn a_router_steers_only_its_own_workers() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let router = job(&store, "router-session", None);
        let mine = job(&store, "w1", Some(&router.identity.job_id));
        let stranger = job(&store, "w2", None);
        let control = HostWorkerControl::new(store.clone(), dir.path().join("projects"));

        let refused = control
            .continue_worker("router-session", &stranger.identity.job_id, "go", None)
            .await
            .unwrap_err();
        assert!(refused.contains("not one of your workers"), "{refused}");
        assert!(control
            .continue_worker("not-a-router", &mine.identity.job_id, "go", None)
            .await
            .unwrap_err()
            .contains("not a background router job"));

        control
            .stop_worker("router-session", &mine.identity.job_id)
            .await
            .unwrap();
        assert_eq!(
            store
                .read_state(&mine.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Stopped
        );
        let revived = control
            .continue_worker("router-session", &mine.identity.job_id, "go", None)
            .await
            .unwrap_err();
        assert!(revived.contains("was stopped"), "{revived}");
    }

    #[tokio::test]
    async fn recall_lists_workers_and_a_handed_over_thread_waits_for_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let router = job(&store, "router-session", None);
        let mut worker = job(&store, "w1", Some(&router.identity.job_id));
        worker.identity.name = "Fix CI".into();
        worker.outcome.summary = Some("opened a pull request\nmore".into());
        store.write_state(&worker).unwrap();
        let control = HostWorkerControl::new(store.clone(), dir.path().join("projects"));

        let listed = control.recall("router-session", None).await.unwrap();
        assert!(listed.contains(&worker.identity.job_id), "{listed}");
        assert!(listed.contains("[idle] Fix CI"), "{listed}");
        assert!(listed.contains("— opened a pull request"), "{listed}");

        let key = control
            .subscribe_thread(
                "router-session",
                "https://github.com/acme/widgets/pull/7",
                &worker.identity.job_id,
            )
            .await
            .unwrap();
        assert_eq!(key, "pr:acme/widgets#7");
        let requests = rebon_automation::threads::take_subscription_requests(
            &rebon_automation::threads::subscription_requests_dir(dir.path()),
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].job_id, worker.identity.job_id);

        let digest = control
            .recall("router-session", Some(&worker.identity.job_id))
            .await
            .unwrap();
        assert!(digest.starts_with("Worker"), "{digest}");
        assert!(
            digest.contains("summary: opened a pull request"),
            "{digest}"
        );
        assert!(control
            .recall("router-session", Some("pr:acme/widgets#7"))
            .await
            .unwrap()
            .contains("not a thread anyone is subscribed to"));
    }

    #[tokio::test]
    async fn a_dispatch_needs_a_real_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        job(&store, "router-session", None);
        let control = HostWorkerControl::new(store, dir.path().join("projects"));
        let refused = control
            .dispatch(
                "router-session",
                WorkerDispatch {
                    project: "relative/path".into(),
                    title: "t".into(),
                    brief: "b".into(),
                    done_when: String::new(),
                    isolate: false,
                    model_profile: None,
                    worker: None,
                },
            )
            .await
            .unwrap_err();
        assert!(refused.contains("not a project directory"), "{refused}");
    }

    #[tokio::test]
    async fn a_task_for_a_stopped_or_stranger_worker_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let router = job(&store, "router-session", None);
        let mut stopped = job(&store, "w1", Some(&router.identity.job_id));
        stopped.process.status = BackgroundJobStatus::Stopped;
        store.write_state(&stopped).unwrap();
        let stranger = job(&store, "w2", None);
        let control = HostWorkerControl::new(store.clone(), dir.path().join("projects"));
        let request = |worker: &str| WorkerDispatch {
            project: dir.path().display().to_string(),
            title: "t".into(),
            brief: "b".into(),
            done_when: String::new(),
            isolate: false,
            model_profile: None,
            worker: Some(worker.to_string()),
        };

        let refused = control
            .dispatch("router-session", request(&stopped.identity.job_id))
            .await
            .unwrap_err();
        assert!(refused.contains("leave `worker` out"), "{refused}");
        let refused = control
            .dispatch("router-session", request(&stranger.identity.job_id))
            .await
            .unwrap_err();
        assert!(refused.contains("not one of your workers"), "{refused}");
        assert!(
            !rebon_work::requests_dir(dir.path()).exists(),
            "nothing was filed"
        );
    }

    #[test]
    fn router_question_replies_withdraw_only_the_exact_auto_mode_question() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut worker = job(&store, "worker-session", Some("router-job"));
        worker.identity.runtime.permission_mode = Some("auto".into());
        worker.process.status = BackgroundJobStatus::NeedsInput;
        worker.process.turn_generation = 7;
        worker.outcome.pending_permission = Some(
            serde_json::from_value(serde_json::json!({
                "queryId": 42, "turnGeneration": 7, "tool": "AskUserQuestion",
                "endpoint": {"pid": 1, "port": 2, "token": "test"},
                "options": [{"optionId": "reject_once", "label": "No", "kind": "RejectOnce"}]
            }))
            .unwrap(),
        );
        let request = router_question_reply(&worker, 42, "Use the authorized patch").unwrap();
        match request {
            rebon_session_host::BackgroundIpcRequest::PermissionAnswer {
                query_id,
                turn_generation,
                option_id,
                extra_text,
                updated_input,
            } => {
                assert_eq!((query_id, turn_generation), (42, 7));
                assert_eq!(option_id.as_deref(), Some("reject_once"));
                assert!(updated_input.is_none(), "never manufactures user answers");
                let guidance = extra_text.unwrap();
                assert!(guidance.contains("Use the authorized patch"));
                assert!(guidance.contains("not user authorization"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
        assert!(router_question_reply(&worker, 41, "stale decision").is_err());
        assert!(router_question_reply(&worker, 42, "  ").is_err());
        for mode in [
            None,
            Some("default"),
            Some("plan"),
            Some("dontAsk"),
            Some("acceptEdits"),
            Some("bypassPermissions"),
        ] {
            worker.identity.runtime.permission_mode = mode.map(str::to_string);
            assert!(
                router_question_reply(&worker, 42, "go").is_err(),
                "{mode:?}"
            );
        }
        worker.identity.runtime.permission_mode = Some("auto".into());
        for tool in [
            "Bash",
            "PowerShell",
            "Agent",
            "ExitPlanMode",
            "Workflow",
            "ProfileSwitch",
        ] {
            worker.outcome.pending_permission.as_mut().unwrap().tool = Some(tool.into());
            assert!(router_question_reply(&worker, 42, "go").is_err(), "{tool}");
        }
        worker.outcome.pending_permission.as_mut().unwrap().tool = Some("AskUserQuestion".into());
        worker.process.turn_generation += 1;
        assert!(router_question_reply(&worker, 42, "old turn").is_err());
        worker.process.turn_generation = 7;
        worker.identity.parent_job_id = None;
        assert!(router_question_reply(&worker, 42, "ordinary auto session").is_err());
    }

    #[test]
    fn recall_exposes_the_pending_question_and_its_fence_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let router = job(&store, "router-session", None);
        let mut worker = job(&store, "worker-session", Some(&router.identity.job_id));
        worker.identity.runtime.permission_mode = Some("auto".into());
        worker.process.status = BackgroundJobStatus::NeedsInput;
        worker.process.turn_generation = 7;
        worker.outcome.pending_permission = Some(
            serde_json::from_value(serde_json::json!({
                "queryId": 42, "turnGeneration": 7, "tool": "AskUserQuestion",
                "title": "Answer questions?", "options": [],
                "toolInput": {"questions": [{"question": "Which patch strategy?",
                    "options": [{"label": "Both repos", "description": "Already authorized"},
                                {"label": "Stop", "description": "Wait again"}]}]}
            }))
            .unwrap(),
        );
        store.write_state(&worker).unwrap();
        let control = HostWorkerControl::new(store.clone(), dir.path().join("projects"));
        let reloaded = store.read_state(&worker.identity.job_id).unwrap();
        let text = control.render_job(&router, &reloaded);
        assert!(text.contains("questionId: 42"), "{text}");
        assert!(text.contains("Which patch strategy?"), "{text}");
        assert!(text.contains("Already authorized"), "{text}");
        assert!(text.contains("not user authorization"), "{text}");
    }

    #[test]
    fn a_worker_brief_delegates_uncertainty_without_reconfirming_authorized_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let router = job(&store, "router-session", None);
        let request = WorkerDispatch {
            project: dir.path().to_string_lossy().into_owned(),
            title: "Patch both repos".into(),
            brief: "Implementation, verification and release are already authorized".into(),
            done_when: "Both patches are verified".into(),
            isolate: false,
            model_profile: None,
            worker: None,
        };
        let text = worker_brief(&router, &request, Some("task-1"));
        assert!(text.contains("Do not ask again"), "{text}");
        assert!(text.contains("not new user authorization"), "{text}");
        assert!(text.contains(&request.brief));
        assert!(text.contains(&request.done_when));
    }

    fn activity(idle_minutes: u64, tokens: u64) -> rebon_group::warmth::Activity {
        rebon_group::warmth::Activity {
            last_activity_ms: Some(1_000_000_000 - idle_minutes * 60_000),
            busy: Some(false),
            context_tokens: Some(tokens),
            ..Default::default()
        }
    }

    #[test]
    fn a_finished_worker_reads_warm_cold_or_long() {
        use rebon_work::WorkerFit;
        let now = 1_000_000_000;
        assert_eq!(fit_of(None, now), WorkerFit::Unknown);
        assert_eq!(fit_of(Some(&activity(5, 100_000)), now), WorkerFit::Warm);
        // Past the cache with more than a short context: it compacts first.
        assert_eq!(fit_of(Some(&activity(90, 100_000)), now), WorkerFit::Cold);
        // A short context is cheap to re-read however long it sat.
        assert_eq!(fit_of(Some(&activity(90, 20_000)), now), WorkerFit::Warm);
        // Past 60% of the window: long, warm or not.
        assert_eq!(fit_of(Some(&activity(5, 150_000)), now), WorkerFit::Long);
        let open_turn = rebon_group::warmth::Activity {
            busy: Some(true),
            ..activity(90, 100_000)
        };
        assert_eq!(fit_of(Some(&open_turn), now), WorkerFit::Cold);
    }

    #[test]
    fn the_session_note_says_idle_time_context_and_what_to_do() {
        let now = 1_000_000_000;
        let note = session_note(&activity(42, 85_400), now).unwrap();
        assert_eq!(
            note,
            "idle 42m, context ~85k tokens; warm: a good one for follow-up work"
        );
        let note = session_note(&activity(180, 100_000), now).unwrap();
        assert!(
            note.starts_with("idle 3h, context ~100k tokens; past its prompt cache"),
            "{note}"
        );
        assert!(session_note(&activity(5, 150_000), now)
            .unwrap()
            .contains("fresh worker"));
        assert_eq!(
            session_note(&rebon_group::warmth::Activity::default(), now),
            None
        );
    }
}
