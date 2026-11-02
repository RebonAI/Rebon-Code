//! What the background supervisor does for its sessions on its own, besides
//! running the rules.
//!
//! It brings back a turn whose worker died in the middle of it. It also asks
//! `rebon_session_host::autonomy` before starting a worker for an automatic
//! turn, which holds one back while too many turns are running or once its
//! session has used the day's tokens. The policy comes from the user's
//! `autonomy` settings (`rebon_config::AutonomySettings`), which are read
//! again every few seconds.
//!
//! What it remembers about a job's crashes lives in `<job dir>/autonomy.json`,
//! and only the supervisor writes that file.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rebon_config::AutonomySettings;
use rebon_session_host::{
    automated_turn_hold, AutomatedTurnHold, AutomatedTurnLimits, BackgroundJobState,
    BackgroundJobStatus, BackgroundStore, JobPlacement, STALE_PID_EXIT_ERROR,
};
use serde::{Deserialize, Serialize};

pub(crate) const LEDGER_FILE_NAME: &str = "autonomy.json";
const SETTINGS_REFRESH_MS: u64 = 5_000;
/// How many recoveries a job gets within [`RECOVERY_WINDOW_MS`]. After that,
/// a crash that keeps coming back is left for the person to look at.
const MAX_RECOVERIES: usize = 2;
const RECOVERY_WINDOW_MS: u64 = 60 * 60 * 1000;
/// How long to wait after a crash before recovering it, indexed by how many
/// recoveries came before it.
const RECOVERY_BACKOFF_MS: [u64; MAX_RECOVERIES] = [30_000, 120_000];

/// The supervisor's record of one job's crashes.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ledger {
    /// When recoveries were queued, within the last window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovered_at_ms: Vec<u64>,
    /// The crash last acted on, by the time its turn ended. It may have been
    /// recovered or given up on; either way it is not acted on again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handled_crash_at_ms: Option<u64>,
}

/// The limits automatic turns start under, from the user's settings.
fn turn_limits(settings: &AutonomySettings) -> AutomatedTurnLimits {
    AutomatedTurnLimits {
        max_running_turns: settings.max_automated_turns,
        daily_token_budget: settings.daily_token_budget,
    }
}

/// The supervisor's autonomy state between ticks. Single-threaded, like
/// `AutomationHost`.
pub(crate) struct Autonomy {
    config_home: PathBuf,
    /// The settings and when they were last read; `None` means they have not
    /// been read yet. Never refreshed when `fixed` is set.
    settings: RefCell<(Option<u64>, AutonomySettings)>,
    fixed: bool,
    /// Jobs being held back, and why, as last logged. A hold is logged once
    /// when it starts, not on every tick it lasts.
    held: RefCell<BTreeMap<String, AutomatedTurnHold>>,
}

impl Autonomy {
    pub(crate) fn new(config_home: PathBuf) -> Self {
        Self {
            config_home,
            settings: RefCell::new((None, AutonomySettings::default())),
            fixed: false,
            held: RefCell::default(),
        }
    }

    #[cfg(test)]
    fn with_settings(settings: AutonomySettings) -> Self {
        Self {
            settings: RefCell::new((Some(0), settings)),
            fixed: true,
            ..Self::new(PathBuf::new())
        }
    }

    fn settings(&self, now_ms: u64) -> AutonomySettings {
        let mut slot = self.settings.borrow_mut();
        let stale = slot
            .0
            .is_none_or(|read_at| now_ms.saturating_sub(read_at) >= SETTINGS_REFRESH_MS);
        if stale && !self.fixed {
            *slot = (
                Some(now_ms),
                rebon_config::saved_autonomy_settings_in_dir(&self.config_home),
            );
        }
        slot.1.clone()
    }

    /// The tick's pass: recover crashed turns. `true` while a recovery waits
    /// out its backoff, which keeps the supervisor up for it.
    pub(crate) fn tick(
        &self,
        store: &BackgroundStore,
        jobs: &[BackgroundJobState],
        now_ms: u64,
    ) -> bool {
        let settings = self.settings(now_ms);
        let waiting = settings.recover_crashed_turns && recover_crashed_turns(store, jobs, now_ms);
        self.held.borrow_mut().retain(|job_id, _| {
            jobs.iter().any(|job| {
                job.identity.job_id == *job_id && job.process.status == BackgroundJobStatus::Queued
            })
        });
        waiting
    }

    /// The supervisor's admission hook: whether a worker may start for this
    /// queued job now. A turn a person asked for always may.
    pub(crate) fn admit(
        &self,
        store: &BackgroundStore,
        job: &BackgroundJobState,
        running_turns: usize,
        now_ms: u64,
    ) -> bool {
        let limits = turn_limits(&self.settings(now_ms));
        let job_id = &job.identity.job_id;
        let mut held = self.held.borrow_mut();
        let Some(hold) = automated_turn_hold(job, running_turns, limits, now_ms) else {
            held.remove(job_id);
            return true;
        };
        if held.insert(job_id.clone(), hold) != Some(hold) {
            let _ = store.append_event(
                job_id,
                "automated_turn_held",
                serde_json::json!({ "reason": hold.reason(), "runningTurns": running_turns }),
            );
        }
        false
    }
}

fn ledger_path(store: &BackgroundStore, job_id: &str) -> PathBuf {
    store.job_dir(job_id).join(LEDGER_FILE_NAME)
}

/// A ledger that is missing or unreadable reads as empty. At worst that
/// gives a job one more recovery than it should have had.
fn read_ledger(path: &Path) -> Ledger {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_ledger(path: &Path, ledger: &Ledger) {
    let written = serde_json::to_vec_pretty(ledger)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| {
            rebon_session::write_file_atomically(path, &bytes).map_err(anyhow::Error::from)
        });
    if let Err(error) = written {
        tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "rebon: could not write a job's autonomy ledger");
    }
}

/// When the job's last turn ended, if that turn ended because its worker
/// died: a background job, with a session to resume and no worker left.
/// A job that a terminal was driving is not one of these, because the person
/// who was there brings it back.
fn crashed_at(job: &BackgroundJobState) -> Option<u64> {
    let crashed = job.process.status == BackgroundJobStatus::Failed
        && job.outcome.error.as_deref() == Some(STALE_PID_EXIT_ERROR)
        && job.lease.placement == JobPlacement::Background
        && job.identity.session_id.is_some()
        && !job.process.removal_reserved
        && !crate::hosted_wakeups::worker_alive(job);
    crashed.then_some(job.process.completed_at_ms).flatten()
}

/// Queue a recovery turn for each crashed job whose backoff has passed.
/// Returns `true` while a recovery is still waiting.
///
/// The turn resumes the same session. The prompt the dead turn had taken up
/// is still at the head of the queue, so it runs again, and the recovery
/// note runs with it. A crash older than the window was found long after the
/// fact, for instance after a reboot, and is left for the person.
fn recover_crashed_turns(
    store: &BackgroundStore,
    jobs: &[BackgroundJobState],
    now_ms: u64,
) -> bool {
    let mut waiting = false;
    for job in jobs {
        let Some(crashed_at) = crashed_at(job) else {
            continue;
        };
        if now_ms.saturating_sub(crashed_at) > RECOVERY_WINDOW_MS {
            continue;
        }
        let job_id = job.identity.job_id.as_str();
        let path = ledger_path(store, job_id);
        let mut ledger = read_ledger(&path);
        if ledger.handled_crash_at_ms == Some(crashed_at) {
            continue;
        }
        ledger
            .recovered_at_ms
            .retain(|at| now_ms.saturating_sub(*at) < RECOVERY_WINDOW_MS);
        let earlier = ledger.recovered_at_ms.len();
        if earlier >= MAX_RECOVERIES {
            ledger.handled_crash_at_ms = Some(crashed_at);
            write_ledger(&path, &ledger);
            let _ = store.append_event(
                job_id,
                "crash_recovery_gave_up",
                serde_json::json!({ "recoveries": earlier }),
            );
            continue;
        }
        if now_ms < crashed_at.saturating_add(RECOVERY_BACKOFF_MS[earlier]) {
            waiting = true;
            continue;
        }
        let attempt = earlier + 1;
        // One id per crash, so a ledger write that failed after the prompt
        // was queued cannot queue it twice.
        let id = format!("u-internal-scheduled-recovery-{crashed_at}");
        let text = rebon_tool::cron::mark_scheduled_prompt(&recovery_prompt(attempt));
        match rebon_session_host::append_background_scheduled_prompt(store, job_id, id, text) {
            Ok(_) => {
                ledger.recovered_at_ms.push(now_ms);
                ledger.handled_crash_at_ms = Some(crashed_at);
                write_ledger(&path, &ledger);
                let _ = store.append_event(
                    job_id,
                    "crash_recovery_queued",
                    serde_json::json!({ "attempt": attempt, "crashedAtMs": crashed_at }),
                );
            }
            Err(error) => {
                tracing::warn!(%job_id, error = %format!("{error:#}"), "rebon: could not queue a crash recovery; retrying");
                waiting = true;
            }
        }
    }
    waiting
}

fn recovery_prompt(attempt: usize) -> String {
    format!(
        "Your previous turn was cut off: the process running this session exited before the \
         turn finished (automatic recovery {attempt} of {MAX_RECOVERIES}). Before redoing \
         anything, check what was already done: files changed, commands run, and tool results \
         in the conversation. Then carry on with the task. If the same step brings the process \
         down again, stop and report what happened instead of retrying it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_session_host::BackgroundRuntimeFields;

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

    /// A background job whose worker died mid-turn at `crashed_at`, already
    /// reconciled the way `list_jobs` leaves it.
    fn crashed_job(store: &BackgroundStore, crashed_at: u64) -> BackgroundJobState {
        let mut job = store
            .create_job("p".into(), PathBuf::from("."), runtime())
            .unwrap();
        job.identity.session_id = Some(format!("sess-{}", job.identity.job_id));
        job.process.status = BackgroundJobStatus::Failed;
        job.process.pid = None;
        job.process.completed_at_ms = Some(crashed_at);
        job.outcome.error = Some(STALE_PID_EXIT_ERROR.to_string());
        store.write_state(&job).unwrap();
        job
    }

    fn crash_again(store: &BackgroundStore, job_id: &str, crashed_at: u64) -> BackgroundJobState {
        let mut job = store.read_state(job_id).unwrap();
        job.process.status = BackgroundJobStatus::Failed;
        job.process.completed_at_ms = Some(crashed_at);
        job.outcome.error = Some(STALE_PID_EXIT_ERROR.to_string());
        store.write_state(&job).unwrap();
        job
    }

    fn events(store: &BackgroundStore, job_id: &str) -> Vec<String> {
        std::fs::read_to_string(store.events_path(job_id))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|event| event["kind"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn a_crashed_turn_is_queued_again_after_its_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = crashed_job(&store, 1_000_000);
        let job_id = job.identity.job_id.clone();

        assert!(recover_crashed_turns(
            &store,
            std::slice::from_ref(&job),
            1_000_000 + 10_000
        ));
        assert_eq!(
            store.read_state(&job_id).unwrap().process.status,
            BackgroundJobStatus::Failed,
            "still backing off"
        );

        assert!(!recover_crashed_turns(
            &store,
            std::slice::from_ref(&job),
            1_000_000 + 30_000
        ));
        let recovered = store.read_state(&job_id).unwrap();
        assert_eq!(recovered.process.status, BackgroundJobStatus::Queued);
        assert_eq!(recovered.outcome.error, None);
        let note = recovered.identity.pending_prompts.last().unwrap();
        assert!(
            note.text.contains("automatic recovery 1 of 2"),
            "{}",
            note.text
        );
        assert!(recovered.next_turn_is_automated());
        assert!(events(&store, &job_id).contains(&"crash_recovery_queued".to_string()));

        // The same crash seen again (the record not yet re-read) is not
        // recovered twice.
        assert!(!recover_crashed_turns(&store, &[job], 1_000_000 + 31_000));
        assert_eq!(
            store
                .read_state(&job_id)
                .unwrap()
                .identity
                .pending_prompts
                .len(),
            1
        );
    }

    #[test]
    fn a_crash_that_keeps_coming_back_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let first = 10_000_000;
        let job = crashed_job(&store, first);
        let job_id = job.identity.job_id.clone();
        recover_crashed_turns(&store, &[job], first + 30_000);

        let second = first + 60_000;
        let job = crash_again(&store, &job_id, second);
        assert!(
            recover_crashed_turns(&store, std::slice::from_ref(&job), second + 30_000),
            "longer backoff"
        );
        recover_crashed_turns(&store, &[job], second + 120_000);
        let queued = store.read_state(&job_id).unwrap();
        assert!(queued
            .identity
            .pending_prompts
            .iter()
            .any(|prompt| prompt.text.contains("automatic recovery 2 of 2")));

        let third = second + 180_000;
        let job = crash_again(&store, &job_id, third);
        assert!(!recover_crashed_turns(&store, &[job], third + 600_000));
        let after = store.read_state(&job_id).unwrap();
        assert_eq!(after.process.status, BackgroundJobStatus::Failed);
        assert!(events(&store, &job_id).contains(&"crash_recovery_gave_up".to_string()));
    }

    #[test]
    fn only_a_background_turn_that_died_recently_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());

        let mut foreground = crashed_job(&store, 1_000);
        foreground.lease.placement = JobPlacement::Foreground;
        let mut failed_on_its_own = crashed_job(&store, 1_000);
        failed_on_its_own.outcome.error = Some("quota exceeded".into());
        let found_after_a_reboot = crashed_job(&store, 1_000);
        let jobs = [foreground, failed_on_its_own];
        assert!(!recover_crashed_turns(&store, &jobs, 31_000));
        assert!(!recover_crashed_turns(
            &store,
            std::slice::from_ref(&found_after_a_reboot),
            1_000 + RECOVERY_WINDOW_MS + 1
        ));
        for job in jobs.iter().chain([&found_after_a_reboot]) {
            assert_eq!(
                store
                    .read_state(&job.identity.job_id)
                    .unwrap()
                    .process
                    .status,
                BackgroundJobStatus::Failed
            );
        }
    }

    #[test]
    fn switched_off_nothing_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = crashed_job(&store, 1_000);
        let autonomy = Autonomy::with_settings(AutonomySettings {
            recover_crashed_turns: false,
            ..AutonomySettings::default()
        });
        assert!(!autonomy.tick(&store, std::slice::from_ref(&job), 60_000));
        assert_eq!(
            store
                .read_state(&job.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Failed
        );
    }

    fn queued_job(store: &BackgroundStore, automated: bool) -> BackgroundJobState {
        let prompt = if automated {
            rebon_tool::cron::mark_scheduled_prompt("nightly review")
        } else {
            "fix the bug".to_string()
        };
        store
            .create_job(prompt, PathBuf::from("."), runtime())
            .unwrap()
    }

    #[test]
    fn automatic_turns_wait_past_the_turn_limit_and_a_persons_never_do() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let autonomy = Autonomy::with_settings(AutonomySettings {
            max_automated_turns: 2,
            ..AutonomySettings::default()
        });
        let automated = queued_job(&store, true);
        let persons = queued_job(&store, false);

        assert!(autonomy.admit(&store, &automated, 1, 0));
        assert!(!autonomy.admit(&store, &automated, 2, 0));
        assert!(!autonomy.admit(&store, &automated, 3, 1_000));
        assert!(autonomy.admit(&store, &persons, 10, 0));
        let held: Vec<_> = events(&store, &automated.identity.job_id)
            .into_iter()
            .filter(|kind| kind == "automated_turn_held")
            .collect();
        assert_eq!(held.len(), 1, "logged once while the hold lasts");
        assert!(autonomy.admit(&store, &automated, 0, 2_000));
    }

    #[test]
    fn a_session_past_its_daily_budget_is_not_started_automatically() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let autonomy = Autonomy::with_settings(AutonomySettings {
            daily_token_budget: Some(1_000),
            ..AutonomySettings::default()
        });
        let now = 1_700_000_000_000;
        let today = rebon_session_host::local_day_number(now);
        let mut job = queued_job(&store, true);
        let turn = rebon_types::Usage {
            input_tokens: 1_000,
            ..Default::default()
        };
        assert!(autonomy.admit(&store, &job, 0, now));
        job.outcome.record_turn_usage(&turn, today);
        assert!(!autonomy.admit(&store, &job, 0, now));
        assert!(events(&store, &job.identity.job_id).contains(&"automated_turn_held".to_string()));

        let mut persons = job.clone();
        persons.identity.prompt = "fix the bug".into();
        assert!(autonomy.admit(&store, &persons, 0, now));
    }
}
