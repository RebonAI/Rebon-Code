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
//! the supervisor is the only writer of the thread table.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use rebon_session_host::{
    BackgroundJobState, BackgroundJobStatus, BackgroundLaunchOptions, BackgroundRuntimeFields,
    BackgroundStore,
};
use rebon_tool::{DispatchedWorker, WorkerControl, WorkerDispatch};

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

    fn launch(
        &self,
        router: &BackgroundJobState,
        request: &WorkerDispatch,
    ) -> Result<DispatchedWorker, String> {
        let project = PathBuf::from(&request.project);
        if !project.is_absolute() || !project.is_dir() {
            return Err(format!(
                "{} is not a project directory; give its absolute path",
                request.project
            ));
        }
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
        let options = BackgroundLaunchOptions {
            prompt: rebon_tool::cron::mark_scheduled_prompt(&worker_brief(router, request)),
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
            name: Some(request.title.clone()),
            agent_type: None,
            parent_job_id: Some(router.identity.job_id.clone()),
        };
        let job = rebon_session_host::launch_background_prompt(
            &self.store,
            options,
            &crate::host::rebon_exe(),
            crate::host::runtime_permissions::ensure_background_runtime_permission_mode_allowed,
        )
        .map_err(|error| format!("{error:#}"))?;
        Ok(DispatchedWorker {
            worker_id: job.identity.job_id,
            note: (!notes.is_empty()).then(|| notes.join("; ")),
        })
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
        if workers.is_empty() {
            return Ok("You have no workers.".to_string());
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
        let job = self
            .store
            .read_state(id)
            .map_err(|_| format!("nothing is known by the id {id}"))?;
        Ok(self.render_job(&router, &job))
    }
}

#[async_trait]
impl WorkerControl for HostWorkerControl {
    async fn dispatch(
        &self,
        router_session_id: &str,
        request: WorkerDispatch,
    ) -> Result<DispatchedWorker, String> {
        let router = self.router(router_session_id)?;
        self.launch(&router, &request)
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

/// What a new worker is told: who sent it and the router's brief.
fn worker_brief(router: &BackgroundJobState, request: &WorkerDispatch) -> String {
    format!(
        "You are a worker dispatched by the Rebon router (job {}). Work on this task on your own; \
         the router reads your final reply, not the person. Finish with a short report: what you \
         did, what is left, and the URL of any pull request you opened.\n\n\
         Task: {}\n\n{}",
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
                    isolate: false,
                    model_profile: None,
                },
            )
            .await
            .unwrap_err();
        assert!(refused.contains("not a project directory"), "{refused}");
    }
}
