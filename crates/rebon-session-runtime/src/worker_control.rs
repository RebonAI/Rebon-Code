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

use async_trait::async_trait;
use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundLaunchOptions, BackgroundRuntimeFields,
    BackgroundStore,
};
use rebon_tool::{DispatchedWorker, PlannedTask, WorkerControl, WorkerDispatch};

/// The permission mode every worker runs in.
const WORKER_PERMISSION_MODE: &str = "auto";
/// How much of a worker's last reply a recall shows. The rest stays in its
/// transcript.
const RECALL_REPLY_CHARS: usize = 3_000;
const RECALL_MAX_WORKERS: usize = 30;

pub struct HostWorkerControl {
    store: BackgroundStore,
    projects_root: PathBuf,
}

impl HostWorkerControl {
    pub fn new(store: BackgroundStore, projects_root: PathBuf) -> Self {
        Self {
            store,
            projects_root,
        }
    }

    /// The automation files live beside the jobs, as the supervisor reads
    /// them.
    fn config_home(&self) -> &Path {
        self.store.root()
    }

    /// The router job whose session is `session_id`.
    fn router(&self, session_id: &str) -> Result<BackgroundJobState, String> {
        self.store
            .list_jobs()
            .map_err(|error| format!("{error:#}"))?
            .into_iter()
            .find(|job| job.identity.session_id.as_deref() == Some(session_id))
            .ok_or_else(|| {
                "this session is not a background router job, so it has no workers".to_string()
            })
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
            planned.push(rebon_work::NewTask {
                id: ids[&task.key].clone(),
                title: task.title.clone(),
                project: task.project.clone(),
                goal: task.goal.clone(),
                done_when: task.done_when.clone(),
                depends_on,
                isolate: task.isolate,
                model_profile: task.model_profile.clone(),
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
        let mut lines = vec![format!("{total} worker(s), most recent first:")];
        for job in workers.iter().take(RECALL_MAX_WORKERS) {
            let summary = job
                .outcome
                .summary
                .as_deref()
                .and_then(|summary| summary.lines().next())
                .map(|line| format!(" — {line}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {} [{}] {} ({}){summary}",
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
        let mut dispatched = launch_worker(&self.store, &router, &request, Some(&task_id))?;
        self.file(&rebon_work::WorkRequest::Bind {
            router_job_id: Some(router.identity.job_id.clone()),
            task: rebon_work::NewTask {
                id: task_id.clone(),
                title: request.title.clone(),
                project: request.project.clone(),
                goal: request.brief.clone(),
                done_when: request.done_when.clone(),
                depends_on: Vec::new(),
                isolate: request.isolate,
                model_profile: request.model_profile.clone(),
            },
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
    ) -> Result<(), String> {
        let router = self.router(router_session_id)?;
        let worker = self.worker(&router, worker_id)?;
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
        let pr = rebon_automation::threads::parse_thread_ref(thread).ok_or_else(|| {
            format!("{thread} is not a thread: write pr:owner/repo#12 or the pull request's URL")
        })?;
        let owner = if worker_id == router.identity.job_id {
            router
        } else {
            self.worker(&router, worker_id)?
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

    #[tokio::test]
    async fn a_router_steers_only_its_own_workers() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let router = job(&store, "router-session", None);
        let mine = job(&store, "w1", Some(&router.identity.job_id));
        let stranger = job(&store, "w2", None);
        let control = HostWorkerControl::new(store.clone(), dir.path().join("projects"));

        let refused = control
            .continue_worker("router-session", &stranger.identity.job_id, "go")
            .await
            .unwrap_err();
        assert!(refused.contains("not one of your workers"), "{refused}");
        assert!(control
            .continue_worker("not-a-router", &mine.identity.job_id, "go")
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
            .continue_worker("router-session", &mine.identity.job_id, "go")
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
                },
            )
            .await
            .unwrap_err();
        assert!(refused.contains("not a project directory"), "{refused}");
    }
}
