//! The scheduling pass: one look at the rules, their state and the jobs,
//! firing whatever is due. The background supervisor runs it every few
//! seconds; launching a session is the caller's, injected as a closure, so
//! this module never names a session host.

use crate::schedule::{advance, arm, time_trigger_due, LocalStamp};
use crate::{render_prompt, AutomationRule, AutomationRules, AutomationRun, AutomationState};
use crate::{AutomationTrigger, PromptContext, RunRequest};

/// Where a job is, as far as rules care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobPhase {
    /// Queued, running, or waiting on a person.
    Active,
    /// Parked with no turn of its own (a warm session).
    Idle,
    Succeeded,
    Failed,
    Stopped,
}

/// One job as the pass sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobView {
    pub job_id: String,
    pub cwd: String,
    pub phase: JobPhase,
    /// When its last turn ended, for a job that has ended one.
    pub completed_at_ms: Option<u64>,
    pub title: String,
    pub summary: Option<String>,
}

/// A session the pass wants started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub rule_id: String,
    pub prompt: String,
    pub cwd: String,
    pub permission_mode: String,
}

/// Start a session; the launched job's id, or why it could not start.
pub type Launch<'a> = dyn FnMut(&LaunchRequest) -> Result<String, String> + 'a;

/// One pass at `now`. `requests` are the Run-now requests taken this pass.
/// Returns whether `state` changed and needs saving.
pub fn run_pass(
    rules: &AutomationRules,
    state: &mut AutomationState,
    jobs: &[JobView],
    requests: &[RunRequest],
    now_ms: u64,
    local: &LocalStamp,
    launch: &mut Launch<'_>,
) -> bool {
    let mut changed = record_outcomes(state, jobs, now_ms);
    changed |= reconcile_arming(rules, state, now_ms, local);
    let mut pass = Pass {
        state,
        jobs,
        now_ms,
        local,
        launch,
    };
    for request in requests {
        if let Some(rule) = rules.rule(&request.rule_id) {
            // The description the desktop app localizes for a manual run.
            pass.fire(rule, "Manual run".to_string(), None, true);
            changed = true;
        }
    }
    if rules.master_enabled {
        changed |= pass.fire_due_clock_rules(rules);
    }
    changed |= pass.fire_event_rules(rules);
    changed
}

/// Note how the jobs rules launched ended. `true` when anything was news.
fn record_outcomes(state: &mut AutomationState, jobs: &[JobView], now_ms: u64) -> bool {
    let mut changed = false;
    for job in jobs {
        let outcome = match job.phase {
            JobPhase::Succeeded => "succeeded",
            JobPhase::Failed => "failed",
            JobPhase::Stopped => "stopped",
            JobPhase::Active | JobPhase::Idle => continue,
        };
        changed |=
            state.record_job_outcome(&job.job_id, outcome, job.completed_at_ms.unwrap_or(now_ms));
    }
    changed
}

/// Arm what was written, switched on, or given a new trigger since the last
/// pass; disarm what was switched off, so switching it on again re-arms it;
/// forget the state of rules that no longer exist.
fn reconcile_arming(
    rules: &AutomationRules,
    state: &mut AutomationState,
    now_ms: u64,
    local: &LocalStamp,
) -> bool {
    let mut changed = false;
    let before = state.rules.len();
    state.rules.retain(|id, _| rules.rule(id).is_some());
    changed |= state.rules.len() != before;
    for rule in &rules.rules {
        let rule_state = state.rules.entry(rule.id.clone()).or_default();
        if !rule.enabled {
            changed |= rule_state.armed_for.take().is_some();
        } else if rule_state.armed_for.as_ref() != Some(&rule.trigger) {
            arm(&rule.trigger, rule_state, now_ms, local);
            changed = true;
        }
    }
    changed
}

struct Pass<'p, 'l> {
    state: &'p mut AutomationState,
    jobs: &'p [JobView],
    now_ms: u64,
    local: &'p LocalStamp,
    launch: &'p mut Launch<'l>,
}

impl Pass<'_, '_> {
    fn fire_due_clock_rules(&mut self, rules: &AutomationRules) -> bool {
        let due: Vec<&AutomationRule> = rules
            .rules
            .iter()
            .filter(|rule| rule.enabled)
            .filter(|rule| {
                self.state.rule(&rule.id).is_some_and(|state| {
                    time_trigger_due(&rule.trigger, state, self.now_ms, self.local)
                })
            })
            .collect();
        for rule in &due {
            self.fire(rule, rule.trigger.summary(), None, false);
        }
        !due.is_empty()
    }

    /// Fire event rules for jobs that ended since the last pass. The first
    /// pass only starts the clock: what ended before it is history.
    fn fire_event_rules(&mut self, rules: &AutomationRules) -> bool {
        let Some(seen_until) = self.state.events_seen_until_ms else {
            self.state.events_seen_until_ms = Some(self.now_ms);
            return true;
        };
        let launched: std::collections::HashSet<String> = self
            .state
            .launched_job_ids()
            .into_iter()
            .map(str::to_string)
            .collect();
        // The slice, not a borrow of `self`: firing needs `self` mutably.
        let jobs = self.jobs;
        let ended: Vec<&JobView> = jobs
            .iter()
            .filter(|job| matches!(job.phase, JobPhase::Succeeded | JobPhase::Failed))
            .filter(|job| job.completed_at_ms.is_some_and(|at| at > seen_until))
            // Automation launches never chain: a failing auto-fix would loop.
            .filter(|job| !launched.contains(&job.job_id))
            .collect();
        let Some(latest) = ended.iter().filter_map(|job| job.completed_at_ms).max() else {
            return false;
        };
        self.state.events_seen_until_ms = Some(latest);
        if !rules.master_enabled {
            return true;
        }
        for job in ended {
            let failed = job.phase == JobPhase::Failed;
            let subject = if job.title.trim().is_empty() {
                job.summary.clone().unwrap_or_default()
            } else {
                job.title.clone()
            };
            let desc = format!(
                "{} · {}",
                if failed {
                    "Session failed"
                } else {
                    "Session done"
                },
                snippet(&subject, 40)
            );
            for rule in rules.rules.iter().filter(|rule| {
                rule.enabled
                    && match rule.trigger {
                        AutomationTrigger::JobFailed => failed,
                        AutomationTrigger::JobSucceeded => !failed,
                        _ => false,
                    }
                    // An explicit project scopes the rule to that project's jobs.
                    && rule.project_cwd.as_deref().is_none_or(|cwd| cwd == job.cwd)
            }) {
                self.fire(rule, desc.clone(), Some(job), false);
            }
        }
        true
    }

    /// Fire one rule: render its prompt, launch, and record the run. A
    /// manual run skips the still-running guard — the user asked.
    fn fire(
        &mut self,
        rule: &AutomationRule,
        trigger: String,
        source: Option<&JobView>,
        manual: bool,
    ) {
        let busy = !manual && self.rule_busy(rule);
        let rule_state = self.state.rules.entry(rule.id.clone()).or_default();
        // The anchor moves whatever happens next, so a clock rule retries a
        // period from now rather than on every pass.
        advance(&rule.trigger, rule_state, self.now_ms, self.local);
        if busy {
            rule_state.stats.skipped_busy += 1;
            return;
        }
        rule_state.stats.total_fires += 1;
        if manual {
            rule_state.stats.manual_fires += 1;
        }
        let mut run = AutomationRun {
            at_ms: self.now_ms,
            trigger: trigger.clone(),
            job_id: None,
            ok: false,
            detail: String::new(),
            outcome: None,
            completed_ms: None,
            manual,
        };
        let cwd = rule
            .project_cwd
            .clone()
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| {
                source
                    .map(|job| job.cwd.clone())
                    .filter(|cwd| !cwd.is_empty())
            });
        let Some(cwd) = cwd else {
            run.detail = "no project to run in".into();
            rule_state.stats.launch_failures += 1;
            rule_state.record_run(run);
            return;
        };
        let prompt = render_prompt(
            &rule.prompt,
            &PromptContext {
                project: project_name(&cwd),
                cwd: cwd.clone(),
                date: self.local.day.clone(),
                time: self.local.time_label.clone(),
                trigger,
                session_title: source.map(|job| job.title.clone()),
                job_summary: source.and_then(|job| job.summary.clone()),
                job_status: source.map(|job| phase_label(job.phase).to_string()),
            },
        );
        if prompt.trim().is_empty() {
            run.detail = "prompt rendered empty".into();
            rule_state.stats.launch_failures += 1;
            rule_state.record_run(run);
            return;
        }
        // A rule firing on its own is stored text, possibly carrying another
        // session's output through `{{job_summary}}`: mark it so the session's
        // auto-mode classifier does not read it as the user asking. A manual
        // run is the user asking, so it goes out as written.
        let prompt = if manual {
            prompt
        } else {
            rebon_tool::cron::mark_scheduled_prompt(&prompt)
        };
        let request = LaunchRequest {
            rule_id: rule.id.clone(),
            prompt,
            cwd,
            permission_mode: rule.permission_mode.clone(),
        };
        match (self.launch)(&request) {
            Ok(job_id) => {
                run.job_id = Some(job_id);
                run.ok = true;
                run.detail = "launched".into();
            }
            Err(error) => {
                run.detail = snippet(&error, 200);
                rule_state.stats.launch_failures += 1;
            }
        }
        rule_state.record_run(run);
    }

    /// The rule's last launch is still going.
    fn rule_busy(&self, rule: &AutomationRule) -> bool {
        self.state
            .rule(&rule.id)
            .and_then(|state| state.latest_job_id())
            .is_some_and(|job_id| {
                self.jobs
                    .iter()
                    .any(|job| job.job_id == job_id && job.phase == JobPhase::Active)
            })
    }
}

fn phase_label(phase: JobPhase) -> &'static str {
    match phase {
        JobPhase::Active => "running",
        JobPhase::Idle => "idle",
        JobPhase::Succeeded => "succeeded",
        JobPhase::Failed => "failed",
        JobPhase::Stopped => "stopped",
    }
}

/// The last path component of `cwd`, or `cwd` itself.
fn project_name(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

/// `text` cut to `max_chars` characters with an ellipsis.
fn snippet(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        flat
    } else {
        let mut cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AutomationRules, LocalStamp, RunRequest};

    const MIN: u64 = 60_000;

    fn stamp() -> LocalStamp {
        LocalStamp {
            day: "2026-11-02".into(),
            minutes_of_day: 600,
            time_label: "10:00".into(),
        }
    }

    fn rule(id: &str, trigger: AutomationTrigger) -> AutomationRule {
        AutomationRule {
            id: id.into(),
            name: id.into(),
            prompt: "check {{project}} ({{trigger}}) {{session_title}} {{job_status}}".into(),
            trigger,
            project_cwd: Some("/work/repo".into()),
            permission_mode: "auto".into(),
            enabled: true,
        }
    }

    fn rules(list: Vec<AutomationRule>) -> AutomationRules {
        AutomationRules {
            rules: list,
            ..Default::default()
        }
    }

    fn job(id: &str, phase: JobPhase, completed_at_ms: Option<u64>) -> JobView {
        JobView {
            job_id: id.into(),
            cwd: "/work/repo".into(),
            phase,
            completed_at_ms,
            title: format!("title of {id}"),
            summary: Some(format!("summary of {id}")),
        }
    }

    /// Runs passes and records every launch the engine asked for.
    struct Harness {
        rules: AutomationRules,
        state: AutomationState,
        launched: Vec<LaunchRequest>,
        fail_launches: bool,
    }

    impl Harness {
        fn new(rules: AutomationRules) -> Self {
            Self {
                rules,
                state: AutomationState::default(),
                launched: Vec::new(),
                fail_launches: false,
            }
        }

        fn pass(&mut self, now_ms: u64, jobs: &[JobView], requests: &[RunRequest]) -> bool {
            let launched = &mut self.launched;
            let fail = self.fail_launches;
            let mut launch = |request: &LaunchRequest| {
                if fail {
                    return Err("rebon binary not found".to_string());
                }
                launched.push(request.clone());
                Ok(format!("job-{}", launched.len()))
            };
            run_pass(
                &self.rules,
                &mut self.state,
                jobs,
                requests,
                now_ms,
                &stamp(),
                &mut launch,
            )
        }

        fn rule_state(&self, id: &str) -> &crate::RuleState {
            self.state.rule(id).expect("tracked")
        }
    }

    #[test]
    fn a_clock_rule_is_armed_first_then_fires_a_marked_prompt_in_its_project() {
        let mut h = Harness::new(rules(vec![rule(
            "r",
            AutomationTrigger::Interval { minutes: 5 },
        )]));
        assert!(h.pass(0, &[], &[]), "arming is a change");
        assert!(h.launched.is_empty(), "never fires on arming");
        h.pass(5 * MIN - 1, &[], &[]);
        assert!(h.launched.is_empty());

        h.pass(5 * MIN, &[], &[]);
        let [request] = h.launched.as_slice() else {
            panic!("one launch: {:?}", h.launched)
        };
        assert_eq!(request.cwd, "/work/repo");
        assert_eq!(request.permission_mode, "auto");
        assert_eq!(
            request.prompt,
            rebon_tool::cron::mark_scheduled_prompt("check repo (Every 5 min)  ")
        );
        let state = h.rule_state("r");
        assert_eq!(state.stats.total_fires, 1);
        assert_eq!(state.runs[0].job_id.as_deref(), Some("job-1"));
        assert!(state.runs[0].ok && !state.runs[0].manual);
        assert_eq!(
            state.last_fired_ms,
            Some(5 * MIN),
            "next period from this fire"
        );
    }

    #[test]
    fn a_rule_whose_last_launch_still_runs_is_skipped_and_retries_a_period_later() {
        let mut h = Harness::new(rules(vec![rule(
            "r",
            AutomationTrigger::Interval { minutes: 5 },
        )]));
        h.pass(0, &[], &[]);
        h.pass(5 * MIN, &[], &[]);
        let running = [job("job-1", JobPhase::Active, None)];
        h.pass(10 * MIN, &running, &[]);
        assert_eq!(h.launched.len(), 1, "skipped");
        assert_eq!(h.rule_state("r").stats.skipped_busy, 1);
        h.pass(10 * MIN + 1, &running, &[]);
        assert_eq!(
            h.rule_state("r").stats.skipped_busy,
            1,
            "not again until a period passes"
        );

        let done = [job("job-1", JobPhase::Succeeded, Some(11 * MIN))];
        h.pass(15 * MIN, &done, &[]);
        assert_eq!(h.launched.len(), 2);
        let state = h.rule_state("r");
        assert_eq!(state.runs[1].outcome.as_deref(), Some("succeeded"));
        assert_eq!(state.runs[1].completed_ms, Some(11 * MIN));
        assert_eq!(state.stats.jobs_succeeded, 1);
    }

    #[test]
    fn run_now_fires_even_while_busy_or_switched_off_and_goes_out_unmarked() {
        let mut list = rules(vec![rule(
            "r",
            AutomationTrigger::Daily {
                hour: 23,
                minute: 0,
            },
        )]);
        list.master_enabled = false;
        let mut h = Harness::new(list);
        let request = |at| RunRequest {
            rule_id: "r".into(),
            requested_at_ms: at,
        };
        h.pass(0, &[], &[request(0)]);
        h.pass(1, &[job("job-1", JobPhase::Active, None)], &[request(1)]);
        assert_eq!(h.launched.len(), 2);
        assert_eq!(
            h.launched[0].prompt, "check repo (Manual run)  ",
            "typed by the user"
        );
        let state = h.rule_state("r");
        assert_eq!((state.stats.total_fires, state.stats.manual_fires), (2, 2));
        assert!(state.runs[0].manual);
        // A request for a rule that is gone does nothing.
        let before = h.launched.len();
        h.pass(
            2,
            &[],
            &[RunRequest {
                rule_id: "gone".into(),
                requested_at_ms: 2,
            }],
        );
        assert_eq!(h.launched.len(), before);
    }

    #[test]
    fn the_master_switch_stops_clock_and_event_fires() {
        let mut list = rules(vec![
            rule("clock", AutomationTrigger::Interval { minutes: 1 }),
            rule("event", AutomationTrigger::JobFailed),
        ]);
        list.master_enabled = false;
        let mut h = Harness::new(list);
        h.pass(0, &[], &[]);
        h.pass(
            10 * MIN,
            &[job("other", JobPhase::Failed, Some(5 * MIN))],
            &[],
        );
        assert!(h.launched.is_empty());
        assert_eq!(
            h.state.events_seen_until_ms,
            Some(5 * MIN),
            "events seen while off are not answered when it is switched back on"
        );
    }

    #[test]
    fn event_rules_answer_jobs_that_end_after_the_first_pass_once() {
        let mut h = Harness::new(rules(vec![
            rule("on-fail", AutomationTrigger::JobFailed),
            rule("on-done", AutomationTrigger::JobSucceeded),
        ]));
        let history = [job("old", JobPhase::Failed, Some(1))];
        h.pass(100, &history, &[]);
        assert!(h.launched.is_empty(), "the first pass starts the clock");

        let failed = [
            job("old", JobPhase::Failed, Some(1)),
            job("fresh", JobPhase::Failed, Some(200)),
        ];
        h.pass(300, &failed, &[]);
        let [request] = h.launched.as_slice() else {
            panic!("only the failure rule: {:?}", h.launched)
        };
        assert_eq!(request.rule_id, "on-fail");
        assert!(
            request
                .prompt
                .contains("check repo (Session failed · title of fresh) title of fresh failed"),
            "{}",
            request.prompt
        );
        h.pass(400, &failed, &[]);
        assert_eq!(h.launched.len(), 1, "a job ending is answered once");

        h.pass(500, &[job("ok", JobPhase::Succeeded, Some(450))], &[]);
        assert_eq!(h.launched.last().unwrap().rule_id, "on-done");
    }

    #[test]
    fn an_event_rule_with_a_project_only_answers_that_projects_jobs() {
        let mut h = Harness::new(rules(vec![rule("r", AutomationTrigger::JobFailed)]));
        h.pass(0, &[], &[]);
        let mut elsewhere = job("x", JobPhase::Failed, Some(10));
        elsewhere.cwd = "/work/other".into();
        h.pass(20, &[elsewhere], &[]);
        assert!(h.launched.is_empty());

        // Without a project it answers any job, and runs where that job ran.
        h.rules.rules[0].project_cwd = None;
        let mut elsewhere = job("y", JobPhase::Failed, Some(30));
        elsewhere.cwd = "/work/other".into();
        h.pass(40, &[elsewhere], &[]);
        assert_eq!(h.launched[0].cwd, "/work/other");
    }

    #[test]
    fn a_launch_by_a_rule_never_fires_an_event_rule() {
        let mut h = Harness::new(rules(vec![
            rule("clock", AutomationTrigger::Interval { minutes: 1 }),
            rule("fixer", AutomationTrigger::JobFailed),
        ]));
        h.pass(0, &[], &[]);
        h.pass(MIN, &[], &[]);
        assert_eq!(h.launched.len(), 1);
        h.pass(
            MIN + 10,
            &[job("job-1", JobPhase::Failed, Some(MIN + 5))],
            &[],
        );
        assert_eq!(
            h.launched.len(),
            1,
            "an auto-fix failing must not fix itself"
        );
        assert_eq!(h.rule_state("clock").stats.jobs_failed, 1);
    }

    #[test]
    fn a_failed_launch_is_recorded_and_counted() {
        let mut h = Harness::new(rules(vec![rule(
            "r",
            AutomationTrigger::Interval { minutes: 1 },
        )]));
        h.fail_launches = true;
        h.pass(0, &[], &[]);
        h.pass(MIN, &[], &[]);
        let state = h.rule_state("r");
        assert_eq!(state.stats.launch_failures, 1);
        assert!(!state.runs[0].ok);
        assert_eq!(state.runs[0].detail, "rebon binary not found");
        assert_eq!(state.runs[0].job_id, None);
    }

    #[test]
    fn a_clock_rule_with_no_project_or_an_empty_prompt_records_why_it_did_not_run() {
        let mut no_project = rule("np", AutomationTrigger::Interval { minutes: 1 });
        no_project.project_cwd = None;
        let mut empty = rule("ep", AutomationTrigger::Interval { minutes: 1 });
        empty.prompt = "  {{session_title}} ".into();
        let mut h = Harness::new(rules(vec![no_project, empty]));
        h.pass(0, &[], &[]);
        h.pass(MIN, &[], &[]);
        assert!(h.launched.is_empty());
        assert_eq!(h.rule_state("np").runs[0].detail, "no project to run in");
        assert_eq!(h.rule_state("ep").runs[0].detail, "prompt rendered empty");
        assert_eq!(h.rule_state("np").stats.launch_failures, 1);
    }

    #[test]
    fn switching_a_rule_off_and_on_or_editing_its_trigger_rearms_it() {
        let mut h = Harness::new(rules(vec![rule(
            "r",
            AutomationTrigger::Interval { minutes: 5 },
        )]));
        h.pass(0, &[], &[]);
        h.rules.rules[0].enabled = false;
        h.pass(MIN, &[], &[]);
        assert_eq!(h.rule_state("r").armed_for, None);
        h.pass(10 * MIN, &[], &[]);
        assert!(h.launched.is_empty(), "off");

        h.rules.rules[0].enabled = true;
        h.pass(20 * MIN, &[], &[]);
        assert!(h.launched.is_empty(), "switched on: armed, not fired");
        h.pass(25 * MIN, &[], &[]);
        assert_eq!(h.launched.len(), 1);

        h.rules.rules[0].trigger = AutomationTrigger::Interval { minutes: 60 };
        h.pass(30 * MIN, &[], &[]);
        assert_eq!(h.launched.len(), 1, "a new trigger re-arms");
        assert_eq!(
            h.rule_state("r").armed_for,
            Some(AutomationTrigger::Interval { minutes: 60 })
        );
    }

    #[test]
    fn a_deleted_rules_state_is_dropped() {
        let mut h = Harness::new(rules(vec![rule(
            "r",
            AutomationTrigger::Interval { minutes: 5 },
        )]));
        h.pass(0, &[], &[]);
        h.rules.rules.clear();
        assert!(h.pass(1, &[], &[]));
        assert!(h.state.rule("r").is_none());
        assert!(!h.pass(2, &[], &[]), "nothing left to change");
    }

    #[test]
    fn snippets_flatten_and_cut_on_characters() {
        assert_eq!(snippet("a  b\nc", 10), "a b c");
        assert_eq!(snippet("修复登录流程的测试", 5), "修复登录…");
        assert_eq!(project_name("/work/repo"), "repo");
        assert_eq!(project_name("/"), "/");
    }
}
