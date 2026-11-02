//! `automation-state.json`: what the rules have done — when each last fired,
//! its runs and counters. Its one writer is the background supervisor, which
//! holds the supervisor lock while it runs; everyone else reads it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::AutomationTrigger;

/// File name of the state file in the config home.
pub const STATE_FILE_NAME: &str = "automation-state.json";

/// Newest-first run records kept per rule (older entries are dropped on push).
pub const MAX_RUNS_KEPT: usize = 20;

/// `<config_home>/automation-state.json`.
pub fn state_path(config_home: &Path) -> PathBuf {
    config_home.join(STATE_FILE_NAME)
}

/// One recorded firing of a rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRun {
    pub at_ms: u64,
    /// What fired it, e.g. `Daily 09:00` or `Session failed · fix login flow`.
    pub trigger: String,
    /// The launched background job, when the launch got that far.
    #[serde(default)]
    pub job_id: Option<String>,
    pub ok: bool,
    /// `launched`, or why it did not.
    pub detail: String,
    /// `succeeded` / `failed` / `stopped` once the launched job was seen to
    /// end; `None` while it runs.
    #[serde(default)]
    pub outcome: Option<String>,
    /// When it was seen to end.
    #[serde(default)]
    pub completed_ms: Option<u64>,
    /// Started by the user's Run now rather than by the rule's trigger.
    #[serde(default)]
    pub manual: bool,
}

/// Lifetime counters for one rule; they survive the run history's cap.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleStats {
    /// Fires that got past the busy guard (schedule, event, or manual).
    #[serde(default)]
    pub total_fires: u64,
    /// Subset of `total_fires` started by Run now.
    #[serde(default)]
    pub manual_fires: u64,
    /// Fires whose launch failed (or whose prompt rendered empty).
    #[serde(default)]
    pub launch_failures: u64,
    /// Fires skipped because the previous launch was still running.
    #[serde(default)]
    pub skipped_busy: u64,
    /// Launched jobs seen reaching `Succeeded`.
    #[serde(default)]
    pub jobs_succeeded: u64,
    /// Launched jobs seen reaching `Failed`.
    #[serde(default)]
    pub jobs_failed: u64,
}

/// What one rule has done.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleState {
    /// The interval / cron anchor: the last fire, or when it was armed.
    #[serde(default)]
    pub last_fired_ms: Option<u64>,
    /// Local `YYYY-MM-DD` of the last daily fire (dedupes same-day catch-up).
    #[serde(default)]
    pub last_fired_day: Option<String>,
    /// The trigger the schedule was armed for while the rule was enabled.
    /// `None` ⇒ not armed: a rule just written, switched on, or given a new
    /// trigger is armed first, so editing a rule never fires it on the spot.
    #[serde(default)]
    pub armed_for: Option<AutomationTrigger>,
    #[serde(default)]
    pub runs: Vec<AutomationRun>,
    #[serde(default)]
    pub stats: RuleStats,
}

impl RuleState {
    /// Prepend a run record, keeping at most [`MAX_RUNS_KEPT`].
    pub fn record_run(&mut self, run: AutomationRun) {
        self.runs.insert(0, run);
        self.runs.truncate(MAX_RUNS_KEPT);
    }

    /// The job the most recent run launched, if it launched one.
    pub fn latest_job_id(&self) -> Option<&str> {
        self.runs.first().and_then(|run| run.job_id.as_deref())
    }
}

/// The whole state file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationState {
    /// Per rule id.
    #[serde(default)]
    pub rules: BTreeMap<String, RuleState>,
    /// Jobs that finished at or before this instant have been considered for
    /// event rules. `None` until the first pass, which starts it at "now" so
    /// a fresh supervisor does not answer history.
    #[serde(default)]
    pub events_seen_until_ms: Option<u64>,
}

#[derive(Serialize)]
struct StateFileOut<'a> {
    version: u32,
    #[serde(flatten)]
    state: &'a AutomationState,
}

/// The runtime fields an older build kept inside each rule of the rules
/// file, read once to seed the state file.
#[derive(Deserialize)]
struct LegacyRulesFile {
    #[serde(default)]
    rules: Vec<LegacyRule>,
}

#[derive(Deserialize)]
struct LegacyRule {
    id: String,
    trigger: AutomationTrigger,
    #[serde(default = "legacy_enabled")]
    enabled: bool,
    #[serde(default)]
    last_fired_ms: Option<u64>,
    #[serde(default)]
    last_fired_day: Option<String>,
    #[serde(default)]
    runs: Vec<AutomationRun>,
    #[serde(default)]
    stats: RuleStats,
}

fn legacy_enabled() -> bool {
    true
}

impl AutomationState {
    /// Read the state file; a missing file is a fresh state.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not an automation state file", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Read the state file, or — when there is none yet — seed it from the
    /// run history an older build kept in the rules file at `rules_path`, so
    /// moving the scheduling into the supervisor keeps every rule's history
    /// and schedule. Only the supervisor calls this.
    pub fn load_or_migrate(path: &Path, rules_path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let Ok(bytes) = std::fs::read(rules_path) else {
            return Ok(Self::default());
        };
        let Ok(legacy) = serde_json::from_slice::<LegacyRulesFile>(&bytes) else {
            return Ok(Self::default());
        };
        let rules = legacy
            .rules
            .into_iter()
            .map(|rule| {
                // An older engine armed a rule the moment it saw it, so a
                // rule it recorded an anchor for was armed for its trigger.
                let armed =
                    rule.enabled && (rule.last_fired_ms.is_some() || rule.last_fired_day.is_some());
                let state = RuleState {
                    last_fired_ms: rule.last_fired_ms,
                    last_fired_day: rule.last_fired_day,
                    armed_for: armed.then_some(rule.trigger),
                    runs: rule.runs,
                    stats: rule.stats,
                };
                (rule.id, state)
            })
            .collect();
        Ok(Self {
            rules,
            events_seen_until_ms: None,
        })
    }

    /// Write the state file atomically.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let body = serde_json::to_vec_pretty(&StateFileOut {
            version: 1,
            state: self,
        })
        .context("serialize the automation state")?;
        rebon_session::write_file_atomically(path, &body)
            .with_context(|| format!("write {}", path.display()))
    }

    pub fn rule(&self, id: &str) -> Option<&RuleState> {
        self.rules.get(id)
    }

    /// Every job any rule launched — so a launched job finishing never fires
    /// an event rule, which would loop (a failing auto-fix fixing itself).
    pub fn launched_job_ids(&self) -> std::collections::HashSet<&str> {
        self.rules
            .values()
            .flat_map(|rule| rule.runs.iter())
            .filter_map(|run| run.job_id.as_deref())
            .collect()
    }

    /// Record how a launched job ended, once. `true` when this was news.
    pub fn record_job_outcome(&mut self, job_id: &str, outcome: &str, completed_ms: u64) -> bool {
        for rule in self.rules.values_mut() {
            let Some(run) = rule
                .runs
                .iter_mut()
                .find(|run| run.job_id.as_deref() == Some(job_id))
            else {
                continue;
            };
            if run.outcome.is_some() {
                return false;
            }
            run.outcome = Some(outcome.to_string());
            run.completed_ms = Some(completed_ms);
            match outcome {
                "succeeded" => rule.stats.jobs_succeeded += 1,
                "failed" => rule.stats.jobs_failed += 1,
                _ => {}
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(at_ms: u64, job_id: Option<&str>) -> AutomationRun {
        AutomationRun {
            at_ms,
            trigger: "Every 5 min".into(),
            job_id: job_id.map(str::to_string),
            ok: true,
            detail: "launched".into(),
            outcome: None,
            completed_ms: None,
            manual: false,
        }
    }

    #[test]
    fn state_round_trips_and_a_missing_file_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        assert_eq!(
            AutomationState::load(&path).unwrap(),
            AutomationState::default()
        );
        let mut state = AutomationState {
            events_seen_until_ms: Some(42),
            ..Default::default()
        };
        state
            .rules
            .entry("a".into())
            .or_default()
            .record_run(run(1, Some("job-1")));
        state.save(&path).unwrap();
        assert_eq!(AutomationState::load(&path).unwrap(), state);
    }

    #[test]
    fn run_history_is_capped_newest_first() {
        let mut rule = RuleState::default();
        for at in 0..(MAX_RUNS_KEPT as u64 + 5) {
            rule.record_run(run(at, None));
        }
        assert_eq!(rule.runs.len(), MAX_RUNS_KEPT);
        assert_eq!(rule.runs[0].at_ms, MAX_RUNS_KEPT as u64 + 4);
    }

    #[test]
    fn a_job_outcome_is_recorded_once_with_its_counter() {
        let mut state = AutomationState::default();
        state
            .rules
            .entry("a".into())
            .or_default()
            .record_run(run(1, Some("job-1")));
        assert!(state.record_job_outcome("job-1", "failed", 9));
        assert!(
            !state.record_job_outcome("job-1", "succeeded", 10),
            "first seen wins"
        );
        assert!(!state.record_job_outcome("job-unknown", "failed", 10));
        let rule = state.rule("a").unwrap();
        assert_eq!(rule.runs[0].outcome.as_deref(), Some("failed"));
        assert_eq!(rule.runs[0].completed_ms, Some(9));
        assert_eq!((rule.stats.jobs_failed, rule.stats.jobs_succeeded), (1, 0));
        assert!(state.launched_job_ids().contains("job-1"));
    }

    #[test]
    fn an_older_rules_file_seeds_the_state_once() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = crate::rules_path(dir.path());
        let path = state_path(dir.path());
        std::fs::write(
            &rules_path,
            serde_json::json!({
                "version": 1,
                "rules": [
                    {
                        "id": "a", "name": "A", "prompt": "p",
                        "trigger": {"type": "interval", "minutes": 5},
                        "last_fired_ms": 10,
                        "runs": [{"at_ms": 10, "trigger": "Every 5 min", "job_id": "job-1",
                                  "ok": true, "detail": "launched"}],
                        "stats": {"total_fires": 3, "jobs_failed": 1}
                    },
                    {
                        "id": "never", "name": "N", "prompt": "p",
                        "trigger": {"type": "daily", "hour": 9, "minute": 0},
                        "enabled": false
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let state = AutomationState::load_or_migrate(&path, &rules_path).unwrap();
        let a = state.rule("a").unwrap();
        assert_eq!(a.last_fired_ms, Some(10));
        assert_eq!(
            a.armed_for,
            Some(AutomationTrigger::Interval { minutes: 5 })
        );
        assert_eq!(a.stats.total_fires, 3);
        assert_eq!(a.runs[0].job_id.as_deref(), Some("job-1"));
        assert_eq!(state.rule("never").unwrap().armed_for, None, "never armed");
        assert_eq!(state.events_seen_until_ms, None);

        // Once a state file exists, it is the truth.
        AutomationState::default().save(&path).unwrap();
        assert_eq!(
            AutomationState::load_or_migrate(&path, &rules_path).unwrap(),
            AutomationState::default()
        );
    }

    #[test]
    fn migrating_with_no_or_unreadable_rules_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let rules_path = crate::rules_path(dir.path());
        let path = state_path(dir.path());
        assert_eq!(
            AutomationState::load_or_migrate(&path, &rules_path).unwrap(),
            AutomationState::default()
        );
        std::fs::write(&rules_path, b"{nope").unwrap();
        assert_eq!(
            AutomationState::load_or_migrate(&path, &rules_path).unwrap(),
            AutomationState::default()
        );
    }
}
