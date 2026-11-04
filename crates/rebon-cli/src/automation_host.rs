//! Automation rules, run by the background supervisor.
//!
//! The supervisor lends this its tick (`SupervisorHooks::scheduled_work`).
//! Every few seconds — at once when a Run now is waiting — it reads the rules
//! the desktop app wrote, runs `rebon_automation::run_pass` against the jobs
//! the tick just reconciled, launches what is due, and writes the state file,
//! which nothing else writes. A rule on a clock keeps the supervisor running
//! with no job to supervise; that is the whole reason the rules moved here
//! from the app, which stopped firing them when its window closed.
//!
//! The same pass routes pull-request threads to the sessions that opened
//! them (`crate::thread_routing`), against the same state file.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use rebon_automation::{
    has_run_requests, local_stamp, requests_dir, rules_path, run_pass, state_path,
    take_run_requests, AutomationRules, AutomationState, JobPhase, JobView, Launch, LaunchRequest,
};

use crate::background::{
    BackgroundJobState, BackgroundJobStatus, BackgroundLaunchOptions, BackgroundRuntimeFields,
    BackgroundStore,
};

/// How often the rules are looked at when nobody asked for a run.
const PASS_INTERVAL_MS: u64 = 5_000;

/// The supervisor's automation state between ticks. Single-threaded: the
/// supervisor loop is one thread, and the hook is `Fn`.
pub(crate) struct AutomationHost {
    config_home: PathBuf,
    /// `None` until the first pass, which runs on the first tick.
    last_pass_ms: Cell<Option<u64>>,
    /// Loaded on the first pass and kept: this process is the state file's
    /// only writer, so its copy is the truth.
    state: RefCell<Option<AutomationState>>,
    /// Whether the last pass found a rule that needs the supervisor kept up.
    resident: Cell<bool>,
    /// The router's job, looked up each tick: threads nobody owns go to it.
    router_job: RefCell<Option<String>>,
}

impl AutomationHost {
    pub(crate) fn new(config_home: PathBuf) -> Self {
        Self {
            config_home,
            last_pass_ms: Cell::new(None),
            state: RefCell::new(None),
            resident: Cell::new(false),
            router_job: RefCell::new(None),
        }
    }

    pub(crate) fn worker_has_responsibility(&self, worker_job_id: &str) -> Option<bool> {
        let slot = self.state.borrow();
        let state = slot.as_ref()?;
        Some(state.threads.values().any(|thread| {
            thread.job_id == worker_job_id
                && thread
                    .last
                    .as_ref()
                    .is_none_or(|last| last.state == rebon_automation::threads::PrState::Open)
        }))
    }

    /// The supervisor tick's hook: a pass when one is due or a Run now is
    /// waiting, launching through the CLI's gated background launch.
    /// `Ok(true)` keeps the supervisor running.
    pub(crate) fn tick(
        &self,
        store: &BackgroundStore,
        jobs: &[BackgroundJobState],
        now_ms: u64,
    ) -> anyhow::Result<bool> {
        let mut launch = |request: &LaunchRequest| {
            if request.to_router {
                return hand_to_router(store, request, now_ms);
            }
            crate::background::launch_background_prompt(store, launch_options(request))
                .map(|job| job.identity.job_id)
                .map_err(|error| format!("{error:#}"))
        };
        let mut observe = |thread: &rebon_automation::threads::PrThread| {
            crate::background::observe_pull_request(std::path::Path::new(&thread.cwd), &thread.pr)
        };
        // Queued as a scheduled turn; this supervisor starts its worker.
        let mut deliver = |job_id: &str, prompt: String| {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = format!(
                "u-internal-scheduled-thread-{now_ms}-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let text = rebon_tool::cron::mark_scheduled_prompt(&prompt);
            rebon_session_host::append_background_scheduled_prompt(store, job_id, id, text)
                .map(|_| ())
        };
        *self.router_job.borrow_mut() =
            crate::router::current_router(store).map(|router| router.identity.job_id);
        self.tick_with(jobs, now_ms, &mut launch, &mut observe, &mut deliver)
    }

    /// [`Self::tick`] with what it does to the world injected.
    pub(crate) fn tick_with(
        &self,
        jobs: &[BackgroundJobState],
        now_ms: u64,
        launch: &mut Launch<'_>,
        observe_pr: &mut crate::thread_routing::ObservePr<'_>,
        deliver: &mut crate::thread_routing::DeliverToJob<'_>,
    ) -> anyhow::Result<bool> {
        let requests_dir = requests_dir(&self.config_home);
        let pass_due = self
            .last_pass_ms
            .get()
            .is_none_or(|last| now_ms.saturating_sub(last) >= PASS_INTERVAL_MS);
        if !pass_due && !has_run_requests(&requests_dir) {
            return Ok(self.resident.get());
        }
        self.last_pass_ms.set(Some(now_ms));
        let rules = AutomationRules::load(&rules_path(&self.config_home))?;
        if !rules.scheduled_by_supervisor() {
            // An app that still fires its rules wrote this file; see
            // `rebon_automation::RULES_VERSION`.
            self.resident.set(false);
            return Ok(false);
        }
        let requests = take_run_requests(&requests_dir);
        let mut slot = self.state.borrow_mut();
        let state = match slot.as_mut() {
            Some(state) => state,
            None => slot.insert(AutomationState::load_or_migrate(
                &state_path(&self.config_home),
                &rules_path(&self.config_home),
            )?),
        };
        let views: Vec<JobView> = jobs.iter().map(job_view).collect();
        // The router's hand-overs first, so this pass already routes to the
        // new owner.
        let mut changed = false;
        for request in rebon_automation::threads::take_subscription_requests(
            &rebon_automation::threads::subscription_requests_dir(&self.config_home),
        ) {
            changed |= rebon_automation::threads::apply_subscription(state, &request);
        }
        changed |= run_pass(
            &rules,
            state,
            &views,
            &requests,
            now_ms,
            &local_stamp(now_ms),
            launch,
        );
        if rules.pr_follow_up {
            changed |= crate::thread_routing::route_pr_threads(
                state,
                jobs,
                self.router_job.borrow().as_deref(),
                now_ms,
                observe_pr,
                deliver,
            );
        }
        if changed {
            state.save(&state_path(&self.config_home))?;
        }
        // An open thread needs a living supervisor to be looked at: its
        // checks may finish hours after the session that opened it.
        let watching_threads = rules.pr_follow_up && !state.threads.is_empty();
        self.resident
            .set(rules.has_armed_time_rule() || watching_threads);
        Ok(self.resident.get())
    }
}

/// A router rule's firing: its prompt becomes the router's next turn, marked
/// as the rule's. The run records the router's job.
fn hand_to_router(
    store: &BackgroundStore,
    request: &LaunchRequest,
    now_ms: u64,
) -> Result<String, String> {
    let router = crate::router::current_router(store)
        .ok_or("no router is running; start one with `rebon router start`")?;
    let rule: String = request
        .rule_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let id = format!("u-internal-scheduled-rule-{rule}-{now_ms}");
    let project = if request.cwd.is_empty() {
        String::new()
    } else {
        format!(" (project {})", request.cwd)
    };
    // The scheduler's marker must stay the opening line; the rule's header
    // goes under it.
    let unmarked = rebon_tool::cron::strip_scheduled_marker(&request.prompt);
    let body = format!("[Rule {} fired{project}]\n{unmarked}", request.rule_id);
    let text = if unmarked.len() == request.prompt.len() {
        body
    } else {
        rebon_tool::cron::mark_scheduled_prompt(&body)
    };
    rebon_session_host::append_background_scheduled_prompt(
        store,
        &router.identity.job_id,
        id,
        text,
    )
    .map_err(|error| format!("{error:#}"))?;
    Ok(router.identity.job_id)
}

/// A job as a rule sees it.
fn job_view(job: &BackgroundJobState) -> JobView {
    let phase = match job.process.status {
        BackgroundJobStatus::Queued
        | BackgroundJobStatus::Running
        | BackgroundJobStatus::NeedsInput => JobPhase::Active,
        BackgroundJobStatus::Idle => JobPhase::Idle,
        BackgroundJobStatus::Succeeded => JobPhase::Succeeded,
        BackgroundJobStatus::Failed => JobPhase::Failed,
        BackgroundJobStatus::Stopped => JobPhase::Stopped,
    };
    JobView {
        job_id: job.identity.job_id.clone(),
        cwd: job.identity.cwd.clone(),
        phase,
        completed_at_ms: job.process.completed_at_ms,
        title: job.identity.name.clone(),
        summary: job.outcome.summary.clone(),
    }
}

/// What a rule's launch asks the background host for: a background job in
/// the rule's project with the rule's permission mode and nothing else, so
/// the worker resolves the rest from config as any new job does.
fn launch_options(request: &LaunchRequest) -> BackgroundLaunchOptions {
    let unmarked = rebon_tool::cron::strip_scheduled_marker(&request.prompt);
    BackgroundLaunchOptions {
        prompt: request.prompt.clone(),
        images: Vec::new(),
        cwd: PathBuf::from(&request.cwd),
        isolate_in_worktree: false,
        require_worktree: false,
        preserve_worktree_on_success: false,
        queue_session: false,
        runtime: BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            // `default` → the worker's own default.
            permission_mode: Some(request.permission_mode.clone())
                .filter(|mode| !mode.is_empty() && mode != "default"),
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        },
        // A scheduled prompt's marker line is for the model; name the job by
        // what the prompt asks instead, as the app always did.
        name: (unmarked.len() != request.prompt.len()).then(|| unmarked.to_string()),
        agent_type: None,
        parent_job_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_automation::{AutomationRule, AutomationTrigger};

    #[test]
    fn thread_responsibility_is_unknown_until_state_is_loaded() {
        let host = AutomationHost::new(PathBuf::new());
        assert_eq!(host.worker_has_responsibility("worker"), None);
        *host.state.borrow_mut() = Some(AutomationState::default());
        assert_eq!(host.worker_has_responsibility("worker"), Some(false));
    }

    #[test]
    fn a_thread_protects_its_owner_until_it_is_closed_or_removed() {
        use rebon_automation::threads::{subscribe_prs, PrRef, PrSnapshot, PrState};
        let host = AutomationHost::new(PathBuf::new());
        let mut state = AutomationState::default();
        subscribe_prs(
            &mut state,
            "worker",
            "project",
            &[PrRef {
                owner: "RebonAI".into(),
                repo: "Rebon-Code".into(),
                number: 42,
                url: "https://github.com/RebonAI/Rebon-Code/pull/42".into(),
            }],
            1,
        );
        *host.state.borrow_mut() = Some(state);
        assert_eq!(host.worker_has_responsibility("worker"), Some(true));
        assert_eq!(host.worker_has_responsibility("other"), Some(false));
        for status in [PrState::Open, PrState::Closed, PrState::Merged] {
            host.state
                .borrow_mut()
                .as_mut()
                .unwrap()
                .threads
                .values_mut()
                .next()
                .unwrap()
                .last = Some(PrSnapshot {
                state: status,
                checks_failed: 0,
                checks_pending: 0,
                changes_requested: false,
                checks_summary: None,
            });
            assert_eq!(
                host.worker_has_responsibility("worker"),
                Some(status == PrState::Open)
            );
        }
        host.state.borrow_mut().as_mut().unwrap().threads.clear();
        assert_eq!(host.worker_has_responsibility("worker"), Some(false));
    }

    fn write_rules(config_home: &std::path::Path, rules: Vec<AutomationRule>) {
        AutomationRules {
            rules,
            ..Default::default()
        }
        .save(&rules_path(config_home))
        .unwrap();
    }

    #[test]
    fn a_router_rule_becomes_the_routers_next_turn_with_the_marker_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let request = LaunchRequest {
            rule_id: "nightly review".into(),
            prompt: rebon_tool::cron::mark_scheduled_prompt("Review open pull requests"),
            cwd: String::new(),
            permission_mode: "default".into(),
            to_router: true,
        };
        let refused = hand_to_router(&store, &request, 1).unwrap_err();
        assert!(refused.contains("no router is running"), "{refused}");

        let mut router = store
            .create_job(
                "/router".into(),
                PathBuf::from("."),
                launch_options(&request).runtime,
            )
            .unwrap();
        router.identity.session_id = Some("router-session".into());
        router.process.status = BackgroundJobStatus::Idle;
        store.write_state(&router).unwrap();
        crate::router::record_router_for_test(&store, &router.identity.job_id);

        assert_eq!(
            hand_to_router(&store, &request, 2).unwrap(),
            router.identity.job_id
        );
        let queued = store.read_state(&router.identity.job_id).unwrap();
        assert_eq!(queued.process.status, BackgroundJobStatus::Queued);
        let text = &queued.identity.pending_prompts[0].text;
        assert!(
            text.starts_with(rebon_tool::cron::SCHEDULED_PROMPT_MARKER),
            "{text}"
        );
        assert!(
            text.contains("[Rule nightly review fired]\nReview open pull requests"),
            "{text}"
        );
    }

    fn interval_rule(id: &str) -> AutomationRule {
        AutomationRule {
            id: id.into(),
            name: id.into(),
            prompt: "check {{project}}".into(),
            trigger: AutomationTrigger::Interval { minutes: 1 },
            project_cwd: Some("/work/repo".into()),
            permission_mode: "default".into(),
            enabled: true,
            target: rebon_automation::RuleTarget::NewSession,
        }
    }

    fn no_prs(
        _: &rebon_automation::threads::PrThread,
    ) -> anyhow::Result<rebon_automation::threads::PrSnapshot> {
        anyhow::bail!("no gh in tests")
    }

    fn no_delivery(_: &str, _: String) -> anyhow::Result<()> {
        Ok(())
    }

    fn recording(
        launched: &mut Vec<LaunchRequest>,
    ) -> impl FnMut(&LaunchRequest) -> Result<String, String> + '_ {
        move |request| {
            launched.push(request.clone());
            Ok(format!("job-{}", launched.len()))
        }
    }

    #[test]
    fn a_clock_rule_fires_from_the_supervisor_and_keeps_it_running() {
        let home = tempfile::tempdir().unwrap();
        write_rules(home.path(), vec![interval_rule("r")]);
        let host = AutomationHost::new(home.path().to_path_buf());
        let mut launched = Vec::new();

        assert!(host
            .tick_with(
                &[],
                1_000,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());
        assert!(launched.is_empty(), "armed, not fired");
        let state = AutomationState::load(&state_path(home.path())).unwrap();
        assert!(
            state.rule("r").unwrap().armed_for.is_some(),
            "the arming was saved"
        );

        assert!(host
            .tick_with(
                &[],
                61_000,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].cwd, "/work/repo");
        let state = AutomationState::load(&state_path(home.path())).unwrap();
        assert_eq!(
            state.rule("r").unwrap().runs[0].job_id.as_deref(),
            Some("job-1")
        );
    }

    #[test]
    fn between_passes_a_tick_only_looks_for_run_now_requests() {
        let home = tempfile::tempdir().unwrap();
        write_rules(home.path(), vec![interval_rule("r")]);
        let host = AutomationHost::new(home.path().to_path_buf());
        let mut launched = Vec::new();
        host.tick_with(
            &[],
            10_000,
            &mut recording(&mut launched),
            &mut no_prs,
            &mut no_delivery,
        )
        .unwrap();

        // Unreadable rules a second later go unnoticed: no pass is due.
        std::fs::write(rules_path(home.path()), b"{nope").unwrap();
        assert!(host
            .tick_with(
                &[],
                11_000,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());

        // A Run now is answered on the next tick, not the next pass.
        write_rules(home.path(), vec![interval_rule("r")]);
        rebon_automation::request_run(&requests_dir(home.path()), "r", 11_500).unwrap();
        host.tick_with(
            &[],
            12_000,
            &mut recording(&mut launched),
            &mut no_prs,
            &mut no_delivery,
        )
        .unwrap();
        assert_eq!(launched.len(), 1);
        assert_eq!(
            launched[0].prompt, "check repo",
            "a manual run goes out unmarked"
        );
    }

    #[test]
    fn with_only_event_rules_or_none_the_supervisor_may_leave() {
        let home = tempfile::tempdir().unwrap();
        let host = AutomationHost::new(home.path().to_path_buf());
        let mut launched = Vec::new();
        assert!(!host
            .tick_with(
                &[],
                0,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());
        let mut rule = interval_rule("e");
        rule.trigger = AutomationTrigger::JobFailed;
        write_rules(home.path(), vec![rule]);
        assert!(!host
            .tick_with(
                &[],
                PASS_INTERVAL_MS,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());
    }

    #[test]
    fn rules_an_older_app_still_fires_are_left_to_it() {
        let home = tempfile::tempdir().unwrap();
        let mut rules = AutomationRules {
            rules: vec![interval_rule("r")],
            ..Default::default()
        };
        rules.version = 1;
        std::fs::write(rules_path(home.path()), serde_json::to_vec(&rules).unwrap()).unwrap();
        rebon_automation::request_run(&requests_dir(home.path()), "r", 0).unwrap();
        let host = AutomationHost::new(home.path().to_path_buf());
        let mut launched = Vec::new();
        assert!(!host
            .tick_with(
                &[],
                0,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());
        assert!(!host
            .tick_with(
                &[],
                120_000,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());
        assert!(
            launched.is_empty(),
            "two schedulers would fire every rule twice"
        );
        assert!(!state_path(home.path()).exists(), "and nothing is written");
        assert!(
            has_run_requests(&requests_dir(home.path())),
            "requests wait"
        );
    }

    /// A job whose summary names a pull request it opened.
    fn job_with_pr(home: &std::path::Path) -> BackgroundJobState {
        let store = BackgroundStore::new(home);
        let mut job = store
            .create_job(
                "p".into(),
                PathBuf::from("/work/repo"),
                launch_options(&LaunchRequest {
                    rule_id: "r".into(),
                    prompt: "p".into(),
                    cwd: "/work/repo".into(),
                    permission_mode: "default".into(),
                    to_router: false,
                })
                .runtime,
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        job.outcome.summary = Some("Opened https://github.com/RebonAI/Rebon-Code/pull/42".into());
        job
    }

    #[test]
    fn pr_follow_up_subscribes_and_keeps_the_supervisor_watching() {
        let home = tempfile::tempdir().unwrap();
        write_rules(home.path(), Vec::new());
        let job = job_with_pr(home.path());
        let host = AutomationHost::new(home.path().to_path_buf());
        let mut launched = Vec::new();
        assert!(
            host.tick_with(
                &[job],
                0,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap(),
            "an open thread needs a living supervisor"
        );
        let state = AutomationState::load(&state_path(home.path())).unwrap();
        assert!(state.threads.contains_key("pr:rebonai/rebon-code#42"));
    }

    #[test]
    fn with_pr_follow_up_off_no_thread_is_kept() {
        let home = tempfile::tempdir().unwrap();
        AutomationRules {
            pr_follow_up: false,
            ..Default::default()
        }
        .save(&rules_path(home.path()))
        .unwrap();
        let job = job_with_pr(home.path());
        let host = AutomationHost::new(home.path().to_path_buf());
        let mut launched = Vec::new();
        assert!(!host
            .tick_with(
                &[job],
                0,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .unwrap());
        assert!(AutomationState::load(&state_path(home.path()))
            .unwrap()
            .threads
            .is_empty());
    }

    #[test]
    fn an_unreadable_rules_file_is_an_error_for_the_supervisor_to_log() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(rules_path(home.path()), b"{nope").unwrap();
        let host = AutomationHost::new(home.path().to_path_buf());
        let mut launched = Vec::new();
        assert!(host
            .tick_with(
                &[],
                0,
                &mut recording(&mut launched),
                &mut no_prs,
                &mut no_delivery
            )
            .is_err());
    }

    #[test]
    fn a_rules_launch_is_a_plain_background_job_named_by_its_prompt() {
        let marked = rebon_tool::cron::mark_scheduled_prompt("review deps");
        let options = launch_options(&LaunchRequest {
            rule_id: "r".into(),
            prompt: marked.clone(),
            cwd: "/work/repo".into(),
            permission_mode: "auto".into(),
            to_router: false,
        });
        assert_eq!(options.prompt, marked, "the model still sees the marker");
        assert_eq!(options.name.as_deref(), Some("review deps"));
        assert_eq!(options.cwd, PathBuf::from("/work/repo"));
        assert_eq!(options.runtime.permission_mode.as_deref(), Some("auto"));
        assert!(!options.isolate_in_worktree && options.parent_job_id.is_none());

        let manual = launch_options(&LaunchRequest {
            rule_id: "r".into(),
            prompt: "review deps".into(),
            cwd: "/w".into(),
            permission_mode: "default".into(),
            to_router: false,
        });
        assert_eq!(manual.name, None, "an unmarked prompt names itself");
        assert_eq!(
            manual.runtime.permission_mode, None,
            "default is the worker's"
        );
    }
}
