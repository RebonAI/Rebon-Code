//! Cron task data model, JSON IO, jitter, and missed-task detection.
//!
//! Pure data model and helpers for the scheduler above this module.
//!
//! Persistence layout: `<project_root>/.rebon/scheduled_tasks.json`.
//! Shape: `{ "tasks": [{ id, cron, prompt, createdAt, lastFiredAt?, recurring?, permanent? }] }`.
//!
//! `durable=false` tasks live in an in-process [`SessionCronStore`] and are
//! intentionally not written to disk. Durable tasks are file-backed under
//! `.rebon/scheduled_tasks.json`. `agent_id` is preserved in the session model
//! for future teammate routing, but scheduler injection is currently limited
//! to the owning process.

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::expr::{compute_next_cron_run, parse_cron_expression};

/// Subdirectory under the project root that holds scheduling state.
pub const REBON_DIR: &str = ".rebon";
/// File name inside `.rebon/` that stores the task list.
pub const CRON_FILE_NAME: &str = "scheduled_tasks.json";

/// A persisted scheduled prompt.
///
/// - `id` — 8 hex chars, UUID-like. Stable across restarts so jitter is
///   deterministic.
/// - `cron` — 5-field expression, re-validated on read.
/// - `prompt` — text enqueued to the model on fire.
/// - `created_at` — epoch ms at creation time. Anchor for missed detection.
/// - `last_fired_at` — epoch ms of most recent fire. `None` for never-fired
///   tasks and for one-shots (those get deleted on fire).
/// - `recurring` — `true` → reschedule after fire; `false`/absent → delete.
/// - `permanent` — reserved; exempts task from `recurring_max_age_ms` expiry.
///   Not writable via `CronCreate` — kept here for on-disk compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronTask {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(
        rename = "lastFiredAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_fired_at: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub recurring: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub permanent: bool,
}

/// A non-durable scheduled prompt scoped to this process/session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCronTask {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: i64,
    pub last_fired_at: Option<i64>,
    pub recurring: bool,
    pub permanent: bool,
    pub durable: bool,
    pub agent_id: Option<String>,
}

impl SessionCronTask {
    pub fn to_cron_task(&self) -> CronTask {
        CronTask {
            id: self.id.clone(),
            cron: self.cron.clone(),
            prompt: self.prompt.clone(),
            created_at: self.created_at,
            last_fired_at: self.last_fired_at,
            recurring: self.recurring,
            permanent: self.permanent,
        }
    }
}

impl From<&CronTask> for SessionCronTask {
    fn from(task: &CronTask) -> Self {
        Self {
            id: task.id.clone(),
            cron: task.cron.clone(),
            prompt: task.prompt.clone(),
            created_at: task.created_at,
            last_fired_at: task.last_fired_at,
            recurring: task.recurring,
            permanent: task.permanent,
            durable: true,
            agent_id: None,
        }
    }
}

/// A turn the agent scheduled for itself (`ScheduleWakeup`): one prompt at
/// one exact time, to the second. A session holds at most one — a self-paced
/// loop re-arms it every turn, and arming again replaces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWakeup {
    pub id: String,
    pub prompt: String,
    /// Why this delay was chosen, in the agent's words; shown to the user.
    pub reason: String,
    pub created_at: i64,
    pub fire_at: i64,
}

/// What a self-paced loop may still spend. It belongs to the loop, not to one
/// wakeup: each fire counts against it, a re-arm inherits it, and only `stop`
/// or a re-arm that names a new budget ends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeupBudget {
    pub started_at: i64,
    /// No wakeup fires after this; the last one is moved up to it.
    pub deadline: i64,
    pub max_wakeups: Option<u32>,
    /// Wakeups of this loop that have fired.
    pub fired: u32,
}

/// The budget a re-arm starts a new loop with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewWakeupBudget {
    pub duration_ms: i64,
    pub max_wakeups: Option<u32>,
}

/// How long a loop that names no budget may run: a day. A loop nobody bounded
/// should end on its own rather than spend a turn an hour indefinitely.
pub const DEFAULT_WAKEUP_BUDGET_MS: i64 = 24 * 60 * 60 * 1000;

/// Why a wakeup was not armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeupRefused {
    /// The loop has fired every wakeup it was allowed.
    WakeupsSpent { fired: u32, max: u32 },
    /// The loop's time is up.
    DeadlinePassed { deadline: i64 },
}

/// The session's wakeup and the loop budget it is counted against, under one
/// lock so the two never disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WakeupSlot {
    armed: Option<SessionWakeup>,
    budget: Option<WakeupBudget>,
}

/// File name of a hosted job's wakeup, in its job directory.
pub const WAKEUP_FILE_NAME: &str = "wakeup.json";

#[derive(Debug, Clone, Default)]
pub struct SessionCronStore {
    tasks: Arc<Mutex<Vec<SessionCronTask>>>,
    wakeup: Arc<Mutex<WakeupSlot>>,
    /// Where the wakeup slot is mirrored, for a session a background job
    /// hosts; see [`Self::persist_wakeup_at`].
    wakeup_file: Arc<Mutex<Option<PathBuf>>>,
}

impl SessionCronStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add(
        &self,
        cron: &str,
        prompt: &str,
        recurring: bool,
        now_ms: i64,
        agent_id: Option<String>,
    ) -> String {
        let id = generate_task_id();
        let task = SessionCronTask {
            id: id.clone(),
            cron: cron.to_string(),
            prompt: prompt.to_string(),
            created_at: now_ms,
            last_fired_at: None,
            recurring,
            permanent: false,
            durable: false,
            agent_id,
        };
        self.tasks
            .lock()
            .expect("session cron store poisoned")
            .push(task);
        id
    }

    pub fn list(&self) -> Vec<SessionCronTask> {
        self.tasks
            .lock()
            .expect("session cron store poisoned")
            .clone()
    }

    pub fn remove(&self, ids: &[String]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let mut tasks = self.tasks.lock().expect("session cron store poisoned");
        let before = tasks.len();
        tasks.retain(|task| !ids.iter().any(|id| id == &task.id));
        before - tasks.len()
    }

    /// Arm the session's wakeup to fire `delay_ms` after `now_ms`, replacing
    /// any wakeup already armed, and counted against the loop's budget.
    ///
    /// `new_budget` starts a new loop with that budget. Without it the
    /// running loop's budget carries on, or — when there is none — a new
    /// loop starts with [`DEFAULT_WAKEUP_BUDGET_MS`]. A wakeup that would land
    /// past the deadline is moved up to it, so the loop gets one last turn to
    /// report in.
    pub fn arm_wakeup(
        &self,
        prompt: &str,
        reason: &str,
        now_ms: i64,
        delay_ms: i64,
        new_budget: Option<NewWakeupBudget>,
    ) -> Result<SessionWakeup, WakeupRefused> {
        let mut slot = self.wakeup.lock().expect("session wakeup slot poisoned");
        let budget = match (new_budget, slot.budget.clone()) {
            (Some(new), _) => WakeupBudget {
                started_at: now_ms,
                deadline: now_ms.saturating_add(new.duration_ms),
                max_wakeups: new.max_wakeups,
                fired: 0,
            },
            (None, Some(running)) => running,
            (None, None) => WakeupBudget {
                started_at: now_ms,
                deadline: now_ms.saturating_add(DEFAULT_WAKEUP_BUDGET_MS),
                max_wakeups: None,
                fired: 0,
            },
        };
        if let Some(max) = budget.max_wakeups.filter(|max| budget.fired >= *max) {
            return Err(WakeupRefused::WakeupsSpent {
                fired: budget.fired,
                max,
            });
        }
        if now_ms >= budget.deadline {
            return Err(WakeupRefused::DeadlinePassed {
                deadline: budget.deadline,
            });
        }
        let wakeup = SessionWakeup {
            id: generate_task_id(),
            prompt: prompt.to_string(),
            reason: reason.to_string(),
            created_at: now_ms,
            fire_at: now_ms.saturating_add(delay_ms).min(budget.deadline),
        };
        slot.armed = Some(wakeup.clone());
        slot.budget = Some(budget);
        self.mirror_wakeup(&slot);
        Ok(wakeup)
    }

    /// The armed wakeup, if any.
    pub fn wakeup(&self) -> Option<SessionWakeup> {
        self.wakeup
            .lock()
            .expect("session wakeup slot poisoned")
            .armed
            .clone()
    }

    /// The running loop's budget, if a loop is running.
    pub fn wakeup_budget(&self) -> Option<WakeupBudget> {
        self.wakeup
            .lock()
            .expect("session wakeup slot poisoned")
            .budget
            .clone()
    }

    /// End the loop: disarm the wakeup and drop its budget. Returns the
    /// wakeup that was armed.
    pub fn clear_wakeup(&self) -> Option<SessionWakeup> {
        let mut slot = self.wakeup.lock().expect("session wakeup slot poisoned");
        let cleared = std::mem::take(&mut *slot).armed;
        self.mirror_wakeup(&slot);
        cleared
    }

    /// Take the wakeup if it is due at `now_ms`, disarming it so it fires
    /// exactly once, and count it against the loop's budget.
    pub fn take_due_wakeup(&self, now_ms: i64) -> Option<SessionWakeup> {
        let mut slot = self.wakeup.lock().expect("session wakeup slot poisoned");
        if !slot
            .armed
            .as_ref()
            .is_some_and(|wakeup| now_ms >= wakeup.fire_at)
        {
            return None;
        }
        if let Some(budget) = slot.budget.as_mut() {
            budget.fired = budget.fired.saturating_add(1);
        }
        let due = slot.armed.take();
        self.mirror_wakeup(&slot);
        due
    }

    /// Mirror the wakeup slot to `path` from now on, for a session a
    /// background job hosts: the supervisor delivers it from there if the
    /// job's worker is gone when it falls due ([`deliver_persisted_wakeup`]).
    ///
    /// A slot this store already holds — handed over from a terminal — is
    /// written out; an empty one takes what the file holds, which is how a
    /// worker started after the last one was reaped picks its loop back up.
    pub fn persist_wakeup_at(&self, path: PathBuf) {
        let mut slot = self.wakeup.lock().expect("session wakeup slot poisoned");
        *self
            .wakeup_file
            .lock()
            .expect("session wakeup file poisoned") = Some(path.clone());
        if *slot == WakeupSlot::default() {
            match read_wakeup_slot(&path) {
                Ok(Some(persisted)) => *slot = persisted,
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    error = %format!("{error:#}"),
                    "[Cron] ignoring an unreadable wakeup file"
                ),
            }
        }
        self.mirror_wakeup(&slot);
    }

    /// Write `slot` to the mirror file, if there is one; an empty slot
    /// removes it. Called with the slot's lock held, so the file never
    /// trails the slot.
    fn mirror_wakeup(&self, slot: &WakeupSlot) {
        let file = self
            .wakeup_file
            .lock()
            .expect("session wakeup file poisoned");
        let Some(path) = file.as_ref() else {
            return;
        };
        if let Err(error) = write_wakeup_slot(path, slot) {
            tracing::warn!(
                path = %path.display(),
                error = %format!("{error:#}"),
                "[Cron] could not mirror the session's wakeup"
            );
        }
    }

    pub fn mark_fired(&self, ids: &[String], fired_at_ms: i64) {
        if ids.is_empty() {
            return;
        }
        let mut tasks = self.tasks.lock().expect("session cron store poisoned");
        for task in tasks.iter_mut() {
            if ids.iter().any(|id| id == &task.id) {
                task.last_fired_at = Some(fired_at_ms);
            }
        }
    }
}

// --- Session schedule handoff ----------------------------------------------

/// Suffix of the file a session's session-only schedule crosses a handoff in,
/// beside its transcript: `<session id>.schedule-handoff.json`.
pub const SESSION_SCHEDULE_HANDOFF_SUFFIX: &str = ".schedule-handoff.json";

/// How long a handed-off schedule waits for the session's next owner. The
/// file only bridges one release and the adopt after it, seconds apart; one
/// nobody took in this long was left by a handoff that never completed, and
/// its tasks were only ever meant to live as long as a process.
pub const SESSION_SCHEDULE_HANDOFF_TTL_MS: i64 = 10 * 60 * 1000;

/// Where `session_id`'s schedule waits between two owners.
pub fn session_schedule_handoff_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let mut path = rebon_session::project_dir_path(projects_root, cwd);
    path.push(format!("{session_id}{SESSION_SCHEDULE_HANDOFF_SUFFIX}"));
    path
}

/// The handoff file's shape. Written by [`SessionCronStore::hand_off`] and
/// read only by [`SessionCronStore::take_handed_off`].
#[derive(Debug, Serialize, Deserialize)]
struct HandedOffSchedule {
    written_at: i64,
    tasks: Vec<SessionCronTask>,
    /// The armed wakeup and the running loop's budget, together.
    wakeup: WakeupSlot,
}

/// What [`SessionCronStore::take_handed_off`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandedOff {
    /// No file: nothing was handed off.
    Nothing,
    /// The file was older than [`SESSION_SCHEDULE_HANDOFF_TTL_MS`], or did
    /// not parse; it was removed and nothing was taken.
    Discarded,
    /// Taken into the store: how many cron tasks, and whether a wakeup.
    Taken { tasks: usize, wakeup: bool },
}

impl SessionCronStore {
    /// Leave this store's session-only schedule at `path` for whichever
    /// process owns the session next. Call it while this process still holds
    /// the session's lock, so the next owner — who reads only after taking
    /// the lock — always finds it. An empty schedule removes any file there
    /// instead, so a stale one cannot outlive this handoff.
    pub fn hand_off(&self, path: &Path, now_ms: i64) -> Result<()> {
        let schedule = HandedOffSchedule {
            written_at: now_ms,
            tasks: self.list(),
            wakeup: self
                .wakeup
                .lock()
                .expect("session wakeup slot poisoned")
                .clone(),
        };
        if schedule.tasks.is_empty() && schedule.wakeup == WakeupSlot::default() {
            return remove_if_present(path);
        }
        let body = serde_json::to_vec(&schedule).context("serialize handed-off schedule")?;
        rebon_session::write_private_file_atomically(path, &body)
            .with_context(|| format!("write handed-off schedule {}", path.display()))
    }

    /// Take the schedule a previous owner left at `path` into this store,
    /// removing the file so it is taken once. Tasks are appended; the wakeup
    /// and its loop's budget are taken only if this store runs no loop of
    /// its own, which a store that is only now becoming the session's never
    /// does.
    pub fn take_handed_off(&self, path: &Path, now_ms: i64) -> Result<HandedOff> {
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HandedOff::Nothing)
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read handed-off schedule {}", path.display()))
            }
        };
        remove_if_present(path)?;
        let Ok(schedule) = serde_json::from_slice::<HandedOffSchedule>(&raw) else {
            tracing::warn!(path = %path.display(), "[Cron] discarding an unreadable handed-off schedule");
            return Ok(HandedOff::Discarded);
        };
        if now_ms.saturating_sub(schedule.written_at) > SESSION_SCHEDULE_HANDOFF_TTL_MS {
            tracing::info!(path = %path.display(), "[Cron] discarding a handed-off schedule nobody took in time");
            return Ok(HandedOff::Discarded);
        }
        let tasks = schedule.tasks.len();
        self.tasks
            .lock()
            .expect("session cron store poisoned")
            .extend(schedule.tasks);
        let mut slot = self.wakeup.lock().expect("session wakeup slot poisoned");
        let take_loop = *slot == WakeupSlot::default();
        let wakeup = take_loop && schedule.wakeup.armed.is_some();
        if take_loop {
            *slot = schedule.wakeup;
            self.mirror_wakeup(&slot);
        }
        Ok(HandedOff::Taken { tasks, wakeup })
    }
}

/// The wakeup slot a hosted job mirrored at `path`; `None` when there is
/// no file.
fn read_wakeup_slot(path: &Path) -> Result<Option<WakeupSlot>> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .with_context(|| format!("{} is not a wakeup file", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn write_wakeup_slot(path: &Path, slot: &WakeupSlot) -> Result<()> {
    if *slot == WakeupSlot::default() {
        return remove_if_present(path);
    }
    let body = serde_json::to_vec(slot).context("serialize the wakeup")?;
    rebon_session::write_private_file_atomically(path, &body)
        .with_context(|| format!("write {}", path.display()))
}

/// Deliver the wakeup a hosted job mirrored at `path` if it is due at
/// `now_ms`: hand it to `queue`, which makes it the job's next turn, and
/// only once that succeeded disarm it and count it against its loop's budget
/// in the file, exactly as a live worker's own tick would. A `queue` that
/// fails leaves the file as it was, so the next pass tries again.
///
/// Only for a job with no live worker — the supervisor's call. While the
/// worker lives it writes the file and fires the wakeup itself; once it is
/// gone the file has no other writer.
pub fn deliver_persisted_wakeup(
    path: &Path,
    now_ms: i64,
    queue: impl FnOnce(&SessionWakeup) -> Result<()>,
) -> Result<Option<SessionWakeup>> {
    let Some(slot) = read_wakeup_slot(path)? else {
        return Ok(None);
    };
    let store = SessionCronStore {
        wakeup: Arc::new(Mutex::new(slot)),
        ..Default::default()
    };
    if !store
        .wakeup()
        .is_some_and(|wakeup| now_ms >= wakeup.fire_at)
    {
        return Ok(None);
    }
    let due = store.wakeup().expect("checked above");
    queue(&due)?;
    store
        .take_due_wakeup(now_ms)
        .expect("still due: nothing else holds this store");
    let slot = store
        .wakeup
        .lock()
        .expect("session wakeup slot poisoned")
        .clone();
    write_wakeup_slot(path, &slot)?;
    Ok(Some(due))
}

/// Whether a wakeup is armed in the file a hosted job mirrored at `path`:
/// one the supervisor may have to deliver, so it must stay up for it.
pub fn persisted_wakeup_armed(path: &Path) -> bool {
    read_wakeup_slot(path)
        .ok()
        .flatten()
        .is_some_and(|slot| slot.armed.is_some())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn cron_disabled() -> bool {
    std::env::var("REBON_DISABLE_CRON")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Jitter tuning for the scheduler. The defaults were chosen to defend the
/// fleet against thundering-herd bursts at round wall-clock times.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CronJitterConfig {
    /// Recurring-task forward delay as a fraction of the gap between fires.
    pub recurring_frac: f64,
    /// Upper bound on recurring forward delay regardless of gap size.
    pub recurring_cap_ms: i64,
    /// One-shot maximum backward lead.
    pub one_shot_max_ms: i64,
    /// One-shot minimum backward lead when the minute-mod gate matches.
    pub one_shot_floor_ms: i64,
    /// Minute divisor: jitter fires on minutes where `minute % N == 0`.
    pub one_shot_minute_mod: u32,
    /// Max age for recurring tasks. `0` → unlimited. Permanent tasks exempt.
    pub recurring_max_age_ms: i64,
}

pub const DEFAULT_CRON_JITTER_CONFIG: CronJitterConfig = CronJitterConfig {
    recurring_frac: 0.1,
    recurring_cap_ms: 15 * 60 * 1000,
    one_shot_max_ms: 90 * 1000,
    one_shot_floor_ms: 0,
    one_shot_minute_mod: 30,
    recurring_max_age_ms: 7 * 24 * 60 * 60 * 1000,
};

// --- Path helpers ----------------------------------------------------------

pub fn cron_dir(project_root: &Path) -> PathBuf {
    project_root.join(REBON_DIR)
}

pub fn cron_file_path(project_root: &Path) -> PathBuf {
    cron_dir(project_root).join(CRON_FILE_NAME)
}

fn ensure_cron_dir(project_root: &Path) -> Result<PathBuf> {
    let dir = cron_dir(project_root);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create cron dir {}", dir.display()))?;
    Ok(dir)
}

// --- Read / write ----------------------------------------------------------

/// Read + parse the task file. Silently skips malformed entries, so one
/// bad task never blocks the whole file. Missing file → empty list. IO
/// errors other than `NotFound` → empty list (logged at debug).
pub fn read_cron_tasks(project_root: &Path) -> Vec<CronTask> {
    let path = cron_file_path(project_root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "[Cron] failed to read scheduled_tasks.json; treating as empty"
            );
            return Vec::new();
        }
    };

    parse_cron_file_body(&raw)
}

fn parse_cron_file_body(raw: &str) -> Vec<CronTask> {
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(err) => {
            tracing::debug!(error = %err, "[Cron] malformed JSON in cron file; treating as empty");
            return Vec::new();
        }
    };

    let arr = match parsed.get("tasks").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for raw_task in arr {
        let Some(obj) = raw_task.as_object() else {
            tracing::debug!(task = %raw_task, "[Cron] skipping non-object task");
            continue;
        };

        let id = obj.get("id").and_then(|v| v.as_str());
        let cron = obj.get("cron").and_then(|v| v.as_str());
        let prompt = obj.get("prompt").and_then(|v| v.as_str());
        let created_at = obj.get("createdAt").and_then(|v| v.as_i64());

        let (Some(id), Some(cron), Some(prompt), Some(created_at)) = (id, cron, prompt, created_at)
        else {
            tracing::debug!(task = %raw_task, "[Cron] skipping malformed task");
            continue;
        };

        if parse_cron_expression(cron).is_none() {
            tracing::debug!(task_id = %id, cron = %cron, "[Cron] skipping task with invalid cron");
            continue;
        }

        out.push(CronTask {
            id: id.to_string(),
            cron: cron.to_string(),
            prompt: prompt.to_string(),
            created_at,
            last_fired_at: obj.get("lastFiredAt").and_then(|v| v.as_i64()),
            recurring: obj
                .get("recurring")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            permanent: obj
                .get("permanent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    out
}

/// Overwrite the task file atomically (tmp + rename) so a crash mid-write
/// can't truncate the on-disk state. Creates `.rebon/` if absent.
pub fn write_cron_tasks(project_root: &Path, tasks: &[CronTask]) -> Result<()> {
    ensure_cron_dir(project_root)?;
    let path = cron_file_path(project_root);

    let body = serde_json::json!({
        "tasks": tasks,
    });
    let mut serialized =
        serde_json::to_string_pretty(&body).context("failed to serialize cron task file")?;
    serialized.push('\n');

    rebon_session::write_file_atomically(&path, serialized.as_bytes())
        .with_context(|| format!("failed to write cron file {}", path.display()))?;
    Ok(())
}

// --- Mutations -------------------------------------------------------------

/// Append a new task. Returns the generated 8-hex-char id. Caller is
/// responsible for having validated the cron string already (tools do this
/// in `validate_input`).
pub fn add_cron_task(
    project_root: &Path,
    cron: &str,
    prompt: &str,
    recurring: bool,
    now_ms: i64,
) -> Result<String> {
    let id = generate_task_id();
    let task = CronTask {
        id: id.clone(),
        cron: cron.to_string(),
        prompt: prompt.to_string(),
        created_at: now_ms,
        last_fired_at: None,
        recurring,
        permanent: false,
    };
    let mut tasks = read_cron_tasks(project_root);
    tasks.push(task);
    write_cron_tasks(project_root, &tasks)?;
    Ok(id)
}

/// Remove tasks by id. No-op on miss (another process may have raced us).
/// Returns the number of tasks actually removed.
pub fn remove_cron_tasks(project_root: &Path, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let tasks = read_cron_tasks(project_root);
    let before = tasks.len();
    let remaining: Vec<CronTask> = tasks
        .into_iter()
        .filter(|t| !ids.iter().any(|id| id == &t.id))
        .collect();
    let removed = before - remaining.len();
    if removed > 0 {
        write_cron_tasks(project_root, &remaining)?;
    }
    Ok(removed)
}

/// Stamp `last_fired_at = fired_at_ms` on all tasks in `ids`. Batched so N
/// fires in one tick = one read-modify-write, not N. No-op on miss.
pub fn mark_cron_tasks_fired(project_root: &Path, ids: &[String], fired_at_ms: i64) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tasks = read_cron_tasks(project_root);
    let mut changed = false;
    for task in tasks.iter_mut() {
        if ids.iter().any(|id| id == &task.id) {
            task.last_fired_at = Some(fired_at_ms);
            changed = true;
        }
    }
    if changed {
        write_cron_tasks(project_root, &tasks)?;
    }
    Ok(())
}

/// List all persisted tasks. This is a thin re-read: the durable store is
/// the task file, and there is no session store behind it.
pub fn list_all_cron_tasks(project_root: &Path) -> Vec<CronTask> {
    read_cron_tasks(project_root)
}

// --- Next-fire + jitter ----------------------------------------------------

/// Convert a local-time `DateTime` to epoch ms.
fn dt_to_ms(dt: DateTime<Local>) -> i64 {
    dt.timestamp_millis()
}

/// Next match for `cron` strictly after `from_ms`. `None` on invalid cron or
/// no match in the next 366 days.
pub fn next_cron_run_ms(cron: &str, from_ms: i64) -> Option<i64> {
    let fields = parse_cron_expression(cron)?;
    let from = Local.timestamp_millis_opt(from_ms).single()?;
    let next = compute_next_cron_run(&fields, from)?;
    Some(dt_to_ms(next))
}

/// Deterministic `[0, 1)` jitter fraction derived from the first 8 hex chars
/// of `task_id`. Non-hex ids fall back to 0 (no jitter).
fn jitter_frac(task_id: &str) -> f64 {
    let head: String = task_id.chars().take(8).collect();
    match u32::from_str_radix(&head, 16) {
        Ok(v) => v as f64 / (u32::MAX as f64 + 1.0),
        Err(_) => 0.0,
    }
}

/// Next fire time for a **recurring** task, with a deterministic forward
/// delay proportional to the gap between fires (capped). Defends against
/// thundering-herd spikes when many sessions schedule the same cron.
pub fn jittered_next_cron_run_ms(
    cron: &str,
    from_ms: i64,
    task_id: &str,
    cfg: &CronJitterConfig,
) -> Option<i64> {
    let t1 = next_cron_run_ms(cron, from_ms)?;
    let t2 = match next_cron_run_ms(cron, t1) {
        Some(v) => v,
        None => return Some(t1),
    };
    let gap = (t2 - t1) as f64;
    let raw_jitter = (jitter_frac(task_id) * cfg.recurring_frac * gap) as i64;
    let jitter = raw_jitter.min(cfg.recurring_cap_ms);
    Some(t1 + jitter)
}

/// Next fire time for a **one-shot** task. When the computed fire time's
/// minute satisfies the `one_shot_minute_mod` gate, fires early by a
/// deterministic `[floor, max)` ms lead. Clamped so a task created inside
/// its own lead window doesn't fire before it was created.
pub fn one_shot_jittered_next_cron_run_ms(
    cron: &str,
    from_ms: i64,
    task_id: &str,
    cfg: &CronJitterConfig,
) -> Option<i64> {
    let t1 = next_cron_run_ms(cron, from_ms)?;
    let t1_dt = Local.timestamp_millis_opt(t1).single()?;
    if cfg.one_shot_minute_mod == 0 || t1_dt.timestamp_subsec_millis() != 0 {
        // Defensive: cron resolution is 1 minute, but guard against bogus mod.
    }
    // Compare against the local minute of the candidate fire time.
    use chrono::Timelike;
    if (t1_dt.minute() % cfg.one_shot_minute_mod) != 0 {
        return Some(t1);
    }
    let range = (cfg.one_shot_max_ms - cfg.one_shot_floor_ms).max(0) as f64;
    let lead = cfg.one_shot_floor_ms + (jitter_frac(task_id) * range) as i64;
    Some((t1 - lead).max(from_ms))
}

/// A task is "missed" when its next run computed from `created_at` is in the
/// past. Covers both one-shot and recurring tasks — a recurring task whose
/// window passed while Rebon was not running is still missed.
pub fn find_missed_tasks(tasks: &[CronTask], now_ms: i64) -> Vec<CronTask> {
    tasks
        .iter()
        .filter(|t| match next_cron_run_ms(&t.cron, t.created_at) {
            Some(next) => next < now_ms,
            None => false,
        })
        .cloned()
        .collect()
}

/// Whether a recurring task has aged past the config limit. Permanent tasks
/// and configs with `recurring_max_age_ms == 0` are always "not aged".
pub fn is_recurring_task_aged(task: &CronTask, now_ms: i64, cfg: &CronJitterConfig) -> bool {
    if !task.recurring || task.permanent || cfg.recurring_max_age_ms == 0 {
        return false;
    }
    (now_ms - task.created_at) > cfg.recurring_max_age_ms
}

// --- Id generator ----------------------------------------------------------

fn generate_task_id() -> String {
    let mut buf = [0u8; 4];
    // Safe to ignore: if getrandom fails (it won't on supported platforms),
    // fall back to time-nanos — the whole point of the 8-hex id is just to
    // be unique per-task, and collisions inside `read→push→write` would need
    // two nanoseconds to line up on the same process.
    if getrandom::getrandom(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf.copy_from_slice(&(nanos as u32).to_le_bytes());
    }
    format!("{:08x}", u32::from_le_bytes(buf))
}

// --- Time helpers ----------------------------------------------------------

/// Epoch ms for "now". Tiny helper so tests / scheduler can agree on a
/// shared source.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    fn tmp_project() -> TempDir {
        let dir = tempfile::Builder::new()
            .prefix(&format!(
                "rebon-cron-tests-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ))
            .tempdir()
            .unwrap();
        dir
    }

    fn budget(duration_ms: i64, max_wakeups: Option<u32>) -> Option<NewWakeupBudget> {
        Some(NewWakeupBudget {
            duration_ms,
            max_wakeups,
        })
    }

    #[test]
    fn a_loop_with_no_budget_named_runs_for_a_day() {
        let store = SessionCronStore::new();
        store.arm_wakeup("x", "y", 1_000, 60_000, None).unwrap();
        assert_eq!(
            store.wakeup_budget(),
            Some(WakeupBudget {
                started_at: 1_000,
                deadline: 1_000 + DEFAULT_WAKEUP_BUDGET_MS,
                max_wakeups: None,
                fired: 0,
            })
        );
    }

    #[test]
    fn rearms_inherit_the_loop_budget_and_each_fire_counts_against_it() {
        let store = SessionCronStore::new();
        store
            .arm_wakeup("x", "y", 0, 60_000, budget(3_600_000, Some(3)))
            .unwrap();
        for fired in 1..=3 {
            store.take_due_wakeup(i64::MAX - 1).expect("fires");
            assert_eq!(store.wakeup_budget().unwrap().fired, fired);
            let rearm = store.arm_wakeup("x", "y", 1_000, 60_000, None);
            if fired < 3 {
                rearm.expect("budget left");
                assert_eq!(
                    store.wakeup_budget().unwrap().deadline,
                    3_600_000,
                    "inherited"
                );
            } else {
                assert_eq!(rearm, Err(WakeupRefused::WakeupsSpent { fired: 3, max: 3 }));
            }
        }
        assert!(
            store.wakeup().is_none(),
            "a refused arm leaves nothing armed"
        );
    }

    #[test]
    fn a_wakeup_past_the_deadline_moves_up_to_it_and_the_next_is_refused() {
        let store = SessionCronStore::new();
        let last = store
            .arm_wakeup("x", "y", 0, 3_600_000, budget(600_000, None))
            .unwrap();
        assert_eq!(last.fire_at, 600_000);
        store
            .take_due_wakeup(600_000)
            .expect("fires at the deadline");
        assert_eq!(
            store.arm_wakeup("x", "y", 600_000, 60_000, None),
            Err(WakeupRefused::DeadlinePassed { deadline: 600_000 })
        );
    }

    #[test]
    fn naming_a_new_budget_starts_a_new_loop() {
        let store = SessionCronStore::new();
        store
            .arm_wakeup("x", "y", 0, 60_000, budget(60_000, Some(1)))
            .unwrap();
        store.take_due_wakeup(i64::MAX).unwrap();
        assert!(store.arm_wakeup("x", "y", 70_000, 60_000, None).is_err());
        store
            .arm_wakeup("x", "y", 70_000, 60_000, budget(600_000, Some(2)))
            .expect("a new budget, a new loop");
        let fresh = store.wakeup_budget().unwrap();
        assert_eq!(
            (fresh.started_at, fresh.fired, fresh.max_wakeups),
            (70_000, 0, Some(2))
        );
    }

    #[test]
    fn a_handoff_carries_the_loops_budget_with_its_spent_wakeups() {
        let dir = tmp_project();
        let path = handoff_path(&dir);
        let old = SessionCronStore::new();
        old.arm_wakeup("x", "y", 0, 60_000, budget(3_600_000, Some(5)))
            .unwrap();
        old.take_due_wakeup(i64::MAX).unwrap();
        old.arm_wakeup("x", "y", 1, 60_000, None).unwrap();
        old.hand_off(&path, 2).unwrap();

        let new = SessionCronStore::new();
        new.take_handed_off(&path, 3).unwrap();
        assert_eq!(new.wakeup_budget(), old.wakeup_budget());
        assert_eq!(new.wakeup_budget().unwrap().fired, 1);
    }

    #[test]
    fn a_spent_loop_with_nothing_armed_is_still_handed_off() {
        // The budget is the loop's: a new owner must not let a spent loop
        // start over just because no wakeup happened to be armed.
        let dir = tmp_project();
        let path = handoff_path(&dir);
        let old = SessionCronStore::new();
        old.arm_wakeup("x", "y", 0, 60_000, budget(3_600_000, Some(1)))
            .unwrap();
        old.take_due_wakeup(i64::MAX).unwrap();
        old.hand_off(&path, 1).unwrap();
        assert!(path.exists());

        let new = SessionCronStore::new();
        assert_eq!(
            new.take_handed_off(&path, 2).unwrap(),
            HandedOff::Taken {
                tasks: 0,
                wakeup: false
            }
        );
        assert!(new.arm_wakeup("x", "y", 3, 60_000, None).is_err());
    }

    fn wakeup_file(dir: &TempDir) -> PathBuf {
        dir.path().join(WAKEUP_FILE_NAME)
    }

    #[test]
    fn a_hosted_jobs_wakeup_is_mirrored_on_every_change() {
        let dir = tmp_project();
        let path = wakeup_file(&dir);
        let store = SessionCronStore::new();
        store.persist_wakeup_at(path.clone());
        assert!(!path.exists(), "nothing armed, nothing written");

        let armed = store
            .arm_wakeup("next", "CI", 0, 60_000, budget(3_600_000, Some(3)))
            .unwrap();
        assert_eq!(read_wakeup_slot(&path).unwrap().unwrap().armed, Some(armed));

        store.take_due_wakeup(60_000).expect("fires");
        let after_fire = read_wakeup_slot(&path)
            .unwrap()
            .expect("the budget is still there");
        assert_eq!(after_fire.armed, None);
        assert_eq!(after_fire.budget.unwrap().fired, 1);

        store.clear_wakeup();
        assert!(!path.exists(), "stop ends the loop, file and all");
    }

    #[test]
    fn a_new_worker_picks_the_loop_back_up_from_the_file() {
        let dir = tmp_project();
        let path = wakeup_file(&dir);
        let reaped = SessionCronStore::new();
        reaped.persist_wakeup_at(path.clone());
        let armed = reaped
            .arm_wakeup("next", "CI", 0, 600_000, budget(3_600_000, Some(5)))
            .unwrap();

        let next = SessionCronStore::new();
        next.persist_wakeup_at(path.clone());
        assert_eq!(next.wakeup(), Some(armed));
        assert_eq!(next.wakeup_budget(), reaped.wakeup_budget());
    }

    #[test]
    fn a_slot_handed_over_from_a_terminal_wins_over_the_file_and_is_written() {
        let dir = tmp_project();
        let path = wakeup_file(&dir);
        std::fs::write(&path, b"{nope").unwrap();
        let store = SessionCronStore::new();
        let handed = store
            .arm_wakeup("from terminal", "why", 0, 60_000, None)
            .unwrap();
        store.persist_wakeup_at(path.clone());
        assert_eq!(store.wakeup(), Some(handed.clone()));
        assert_eq!(
            read_wakeup_slot(&path).unwrap().unwrap().armed,
            Some(handed)
        );
    }

    #[test]
    fn an_unreadable_wakeup_file_is_ignored_and_replaced() {
        let dir = tmp_project();
        let path = wakeup_file(&dir);
        std::fs::write(&path, b"{nope").unwrap();
        let store = SessionCronStore::new();
        store.persist_wakeup_at(path.clone());
        assert!(store.wakeup().is_none());
        assert!(!path.exists(), "an empty slot is mirrored as no file");
    }

    #[test]
    fn the_supervisor_delivers_a_due_wakeup_once_and_keeps_its_budget() {
        let dir = tmp_project();
        let path = wakeup_file(&dir);
        let worker = SessionCronStore::new();
        worker.persist_wakeup_at(path.clone());
        let armed = worker
            .arm_wakeup("next", "CI", 0, 60_000, budget(3_600_000, Some(2)))
            .unwrap();
        drop(worker); // reaped

        assert!(persisted_wakeup_armed(&path));
        assert_eq!(
            deliver_persisted_wakeup(&path, 59_999, |_| Ok(())).unwrap(),
            None,
            "early"
        );
        assert_eq!(
            deliver_persisted_wakeup(&path, 60_000, |_| Ok(())).unwrap(),
            Some(armed)
        );
        assert_eq!(
            deliver_persisted_wakeup(&path, 120_000, |_| Ok(())).unwrap(),
            None,
            "once"
        );
        assert!(
            !persisted_wakeup_armed(&path),
            "nothing left to hold the supervisor for"
        );

        // The worker it wakes inherits the spent wakeup.
        let revived = SessionCronStore::new();
        revived.persist_wakeup_at(path.clone());
        assert_eq!(revived.wakeup_budget().unwrap().fired, 1);
        revived.arm_wakeup("next", "CI", 1, 60_000, None).unwrap();
        revived.take_due_wakeup(i64::MAX).unwrap();
        assert_eq!(
            revived.arm_wakeup("next", "CI", 2, 60_000, None),
            Err(WakeupRefused::WakeupsSpent { fired: 2, max: 2 }),
            "a reap cannot reset a loop's budget"
        );
    }

    #[test]
    fn delivering_with_no_file_or_a_bad_one() {
        let dir = tmp_project();
        let path = wakeup_file(&dir);
        assert_eq!(
            deliver_persisted_wakeup(&path, 0, |_| Ok(())).unwrap(),
            None
        );
        std::fs::write(&path, b"{nope").unwrap();
        assert!(deliver_persisted_wakeup(&path, 0, |_| Ok(())).is_err());
    }

    #[test]
    fn a_wakeup_that_could_not_be_queued_stays_armed_for_the_next_pass() {
        let dir = tmp_project();
        let path = wakeup_file(&dir);
        let worker = SessionCronStore::new();
        worker.persist_wakeup_at(path.clone());
        let armed = worker.arm_wakeup("next", "CI", 0, 1_000, None).unwrap();
        let failed = deliver_persisted_wakeup(&path, 5_000, |_| Err(anyhow::anyhow!("job busy")));
        assert!(failed.is_err());
        assert!(persisted_wakeup_armed(&path), "nothing was spent");
        assert_eq!(
            deliver_persisted_wakeup(&path, 6_000, |_| Ok(())).unwrap(),
            Some(armed)
        );
    }

    #[test]
    fn a_store_without_a_file_writes_nothing() {
        let dir = tmp_project();
        let store = SessionCronStore::new();
        store.arm_wakeup("x", "y", 0, 60_000, None).unwrap();
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_wakeup_fires_once_at_its_second_and_not_before() {
        let store = SessionCronStore::new();
        assert!(store.wakeup().is_none());
        let armed = store
            .arm_wakeup("check CI", "CI takes ~5m", 1_000, 90_000, None)
            .unwrap();
        assert_eq!(armed.fire_at, 91_000);
        assert_eq!(store.wakeup(), Some(armed.clone()));

        assert!(
            store.take_due_wakeup(90_999).is_none(),
            "a millisecond early"
        );
        assert_eq!(store.wakeup(), Some(armed.clone()), "not due stays armed");
        assert_eq!(store.take_due_wakeup(91_000), Some(armed));
        assert!(store.wakeup().is_none(), "firing disarms it");
        assert!(
            store.take_due_wakeup(200_000).is_none(),
            "and it fires once"
        );
    }

    #[test]
    fn arming_a_wakeup_again_replaces_the_one_armed() {
        let store = SessionCronStore::new();
        let first = store.arm_wakeup("first", "a", 0, 60_000, None).unwrap();
        let second = store
            .arm_wakeup("second", "b", 10_000, 120_000, None)
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(store.wakeup(), Some(second.clone()));
        assert!(store.take_due_wakeup(60_000).is_none(), "the first is gone");
        assert_eq!(store.take_due_wakeup(130_000), Some(second));
    }

    #[test]
    fn clearing_a_wakeup_disarms_it_and_leaves_cron_tasks_alone() {
        let store = SessionCronStore::new();
        let task = store.add("*/5 * * * *", "poll", true, 0, None);
        let armed = store.arm_wakeup("later", "why", 0, 60_000, None).unwrap();
        assert_eq!(store.clear_wakeup(), Some(armed));
        assert!(store.clear_wakeup().is_none());
        assert!(store.take_due_wakeup(i64::MAX).is_none());
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].id, task);
    }

    #[test]
    fn a_wakeup_far_in_the_future_does_not_overflow() {
        let store = SessionCronStore::new();
        let armed = store
            .arm_wakeup("x", "y", i64::MAX - 5, 60_000, None)
            .unwrap();
        assert_eq!(armed.fire_at, i64::MAX);
    }

    #[test]
    fn clones_of_a_store_share_one_wakeup_slot() {
        // The scheduler and the tools hold the same `Arc`, and a worker's
        // held cron hands out clones: they must all see one slot.
        let store = SessionCronStore::new();
        let clone = (*store).clone();
        let armed = clone.arm_wakeup("x", "y", 0, 1, None).unwrap();
        assert_eq!(store.wakeup(), Some(armed));
    }

    fn handoff_path(dir: &TempDir) -> PathBuf {
        let path = session_schedule_handoff_path(dir.path(), "/repo", "sess-1");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    #[test]
    fn a_handed_off_schedule_is_taken_whole_by_the_next_owner_once() {
        let dir = tmp_project();
        let path = handoff_path(&dir);
        let old = SessionCronStore::new();
        let task = old.add("*/5 * * * *", "poll", true, 1_000, None);
        let wakeup = old
            .arm_wakeup("next iteration", "CI", 1_000, 600_000, None)
            .unwrap();
        old.hand_off(&path, 2_000).unwrap();
        assert!(path.exists());

        let new = SessionCronStore::new();
        assert_eq!(
            new.take_handed_off(&path, 3_000).unwrap(),
            HandedOff::Taken {
                tasks: 1,
                wakeup: true
            }
        );
        assert_eq!(new.list(), old.list());
        assert_eq!(new.list()[0].id, task);
        assert_eq!(new.wakeup(), Some(wakeup), "same id and fire time");
        assert!(!path.exists(), "taken once");
        assert_eq!(
            new.take_handed_off(&path, 3_000).unwrap(),
            HandedOff::Nothing
        );
    }

    #[test]
    fn handing_off_an_empty_schedule_clears_a_stale_file() {
        let dir = tmp_project();
        let path = handoff_path(&dir);
        let full = SessionCronStore::new();
        full.arm_wakeup("x", "y", 0, 60_000, None).unwrap();
        full.hand_off(&path, 0).unwrap();
        SessionCronStore::new().hand_off(&path, 1).unwrap();
        assert!(!path.exists());
        // And with no file at all it is a no-op, not an error.
        SessionCronStore::new().hand_off(&path, 2).unwrap();
    }

    #[test]
    fn a_handed_off_schedule_nobody_took_in_time_is_discarded() {
        let dir = tmp_project();
        let path = handoff_path(&dir);
        let old = SessionCronStore::new();
        old.arm_wakeup("x", "y", 0, 60_000, None).unwrap();
        old.hand_off(&path, 0).unwrap();

        let new = SessionCronStore::new();
        let late = SESSION_SCHEDULE_HANDOFF_TTL_MS + 1;
        assert_eq!(
            new.take_handed_off(&path, late).unwrap(),
            HandedOff::Discarded
        );
        assert!(new.wakeup().is_none());
        assert!(!path.exists(), "debris is removed");
    }

    #[test]
    fn a_handed_off_schedule_at_exactly_the_ttl_is_still_taken() {
        let dir = tmp_project();
        let path = handoff_path(&dir);
        let old = SessionCronStore::new();
        old.add("0 9 * * *", "daily", true, 0, None);
        old.hand_off(&path, 0).unwrap();
        let taken = SessionCronStore::new()
            .take_handed_off(&path, SESSION_SCHEDULE_HANDOFF_TTL_MS)
            .unwrap();
        assert_eq!(
            taken,
            HandedOff::Taken {
                tasks: 1,
                wakeup: false
            }
        );
    }

    #[test]
    fn an_unreadable_handoff_file_is_discarded_not_an_error() {
        let dir = tmp_project();
        let path = handoff_path(&dir);
        fs::write(&path, b"{not json").unwrap();
        let new = SessionCronStore::new();
        assert_eq!(new.take_handed_off(&path, 0).unwrap(), HandedOff::Discarded);
        assert!(!path.exists());
    }

    #[test]
    fn taking_a_handoff_keeps_a_wakeup_already_armed_and_appends_tasks() {
        let dir = tmp_project();
        let path = handoff_path(&dir);
        let old = SessionCronStore::new();
        old.add("*/5 * * * *", "old task", true, 0, None);
        old.arm_wakeup("old wakeup", "a", 0, 60_000, None).unwrap();
        old.hand_off(&path, 0).unwrap();

        let new = SessionCronStore::new();
        new.add("*/10 * * * *", "own task", true, 0, None);
        let own = new.arm_wakeup("own wakeup", "b", 0, 120_000, None).unwrap();
        assert_eq!(
            new.take_handed_off(&path, 1).unwrap(),
            HandedOff::Taken {
                tasks: 1,
                wakeup: false
            }
        );
        assert_eq!(new.wakeup(), Some(own));
        let prompts: Vec<_> = new.list().into_iter().map(|t| t.prompt).collect();
        assert_eq!(prompts, ["own task", "old task"]);
    }

    #[test]
    fn handoff_paths_sit_beside_the_sessions_transcript() {
        let root = Path::new("/projects");
        let path = session_schedule_handoff_path(root, "/repo", "sess-1");
        assert_eq!(
            path.parent(),
            Some(rebon_session::project_dir_path(root, "/repo").as_path())
        );
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "sess-1.schedule-handoff.json"
        );
    }

    #[test]
    fn read_missing_file_is_empty() {
        let dir = tmp_project();
        let tasks = read_cron_tasks(dir.path());
        assert!(tasks.is_empty());
    }

    #[test]
    fn roundtrip_write_read_preserves_fields() {
        let dir = tmp_project();
        let tasks = vec![
            CronTask {
                id: "aaaabbbb".into(),
                cron: "0 9 * * *".into(),
                prompt: "daily".into(),
                created_at: 1_700_000_000_000,
                last_fired_at: None,
                recurring: true,
                permanent: false,
            },
            CronTask {
                id: "11112222".into(),
                cron: "*/5 * * * *".into(),
                prompt: "every five".into(),
                created_at: 1_700_000_100_000,
                last_fired_at: Some(1_700_000_500_000),
                recurring: false,
                permanent: false,
            },
        ];
        write_cron_tasks(dir.path(), &tasks).unwrap();
        let read_back = read_cron_tasks(dir.path());
        assert_eq!(read_back, tasks);
    }

    #[test]
    fn bad_json_is_dropped_not_errored() {
        let dir = tmp_project();
        fs::create_dir_all(dir.path().join(".rebon")).unwrap();
        fs::write(
            dir.path().join(".rebon").join("scheduled_tasks.json"),
            "{not json",
        )
        .unwrap();
        assert!(read_cron_tasks(dir.path()).is_empty());
    }

    #[test]
    fn malformed_task_entries_are_silently_skipped() {
        let dir = tmp_project();
        fs::create_dir_all(dir.path().join(".rebon")).unwrap();
        fs::write(
            dir.path().join(".rebon").join("scheduled_tasks.json"),
            r#"{"tasks":[
                {"id":"ok1","cron":"0 9 * * *","prompt":"p","createdAt":1},
                {"id":"missing-prompt","cron":"0 9 * * *","createdAt":2},
                {"id":"bad-cron","cron":"nope","prompt":"p","createdAt":3},
                "not-an-object",
                {"id":"ok2","cron":"*/5 * * * *","prompt":"q","createdAt":4}
            ]}"#,
        )
        .unwrap();

        let tasks = read_cron_tasks(dir.path());
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "ok1");
        assert_eq!(tasks[1].id, "ok2");
    }

    #[test]
    fn add_and_remove_cron_task() {
        let dir = tmp_project();
        let id1 = add_cron_task(dir.path(), "0 9 * * *", "morning", true, 1_000).unwrap();
        let id2 = add_cron_task(dir.path(), "*/15 * * * *", "frequent", false, 2_000).unwrap();
        assert_eq!(id1.len(), 8);
        assert_ne!(id1, id2);
        let tasks = read_cron_tasks(dir.path());
        assert_eq!(tasks.len(), 2);

        let removed = remove_cron_tasks(dir.path(), &[id1.clone()]).unwrap();
        assert_eq!(removed, 1);
        let after = read_cron_tasks(dir.path());
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, id2);

        // Removing a non-existent id is a no-op.
        let gone = remove_cron_tasks(dir.path(), &["doesntexist".into()]).unwrap();
        assert_eq!(gone, 0);
    }

    #[test]
    fn mark_cron_tasks_fired_stamps_last_fired_at() {
        let dir = tmp_project();
        let id = add_cron_task(dir.path(), "0 9 * * *", "prompt", true, 1_000).unwrap();
        mark_cron_tasks_fired(dir.path(), &[id.clone()], 5_000).unwrap();
        let tasks = read_cron_tasks(dir.path());
        assert_eq!(tasks[0].last_fired_at, Some(5_000));

        // Miss is silent.
        mark_cron_tasks_fired(dir.path(), &["nosuch".into()], 7_000).unwrap();
        let tasks = read_cron_tasks(dir.path());
        assert_eq!(tasks[0].last_fired_at, Some(5_000));
    }

    #[test]
    fn next_cron_run_returns_none_on_invalid() {
        assert!(next_cron_run_ms("not a cron", 0).is_none());
    }

    #[test]
    fn jitter_frac_is_deterministic_per_id() {
        let a = jitter_frac("abcdef12");
        let b = jitter_frac("abcdef12");
        assert_eq!(a, b);
        assert!(a >= 0.0 && a < 1.0);
        assert_eq!(jitter_frac("non-hex!"), 0.0);
    }

    #[test]
    fn jittered_recurring_capped_by_config() {
        // Hourly — gap is 1h, at 10% = 6min, capped at 15min means cap doesn't bite.
        let cron = "0 * * * *";
        let from = chrono::Local
            .with_ymd_and_hms(2024, 1, 1, 12, 5, 0)
            .unwrap()
            .timestamp_millis();
        let id = "ffffffff"; // max frac
        let jittered =
            jittered_next_cron_run_ms(cron, from, id, &DEFAULT_CRON_JITTER_CONFIG).unwrap();
        let base = next_cron_run_ms(cron, from).unwrap();
        let delay = jittered - base;
        assert!(delay > 0 && delay <= DEFAULT_CRON_JITTER_CONFIG.recurring_cap_ms);
        // Same inputs → same output (determinism).
        let again = jittered_next_cron_run_ms(cron, from, id, &DEFAULT_CRON_JITTER_CONFIG).unwrap();
        assert_eq!(jittered, again);
    }

    #[test]
    fn one_shot_jitter_gated_by_minute_mod() {
        // Minute 15 does not match mod 30, so no jitter.
        let cron = "15 * * * *";
        let from = chrono::Local
            .with_ymd_and_hms(2024, 1, 1, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        let base = next_cron_run_ms(cron, from).unwrap();
        let jittered =
            one_shot_jittered_next_cron_run_ms(cron, from, "ffffffff", &DEFAULT_CRON_JITTER_CONFIG)
                .unwrap();
        assert_eq!(base, jittered);

        // Minute 30 hits the mod 30 gate.
        let cron_on_mark = "30 * * * *";
        let base2 = next_cron_run_ms(cron_on_mark, from).unwrap();
        let jittered2 = one_shot_jittered_next_cron_run_ms(
            cron_on_mark,
            from,
            "ffffffff",
            &DEFAULT_CRON_JITTER_CONFIG,
        )
        .unwrap();
        assert!(jittered2 < base2);
        assert!((base2 - jittered2) <= DEFAULT_CRON_JITTER_CONFIG.one_shot_max_ms);
    }

    #[test]
    fn one_shot_jitter_clamped_to_from_ms() {
        // Task created 10 ms before its fire time, mod-gated, huge max lead:
        // the lead would want to push fire before `from`, but clamp kicks in.
        let cron = "0 * * * *";
        let base_fire = chrono::Local
            .with_ymd_and_hms(2024, 1, 1, 13, 0, 0)
            .unwrap()
            .timestamp_millis();
        let from = base_fire - 10;
        let cfg = CronJitterConfig {
            one_shot_max_ms: 60_000,
            one_shot_floor_ms: 60_000,
            ..DEFAULT_CRON_JITTER_CONFIG
        };
        let jittered = one_shot_jittered_next_cron_run_ms(cron, from, "00000001", &cfg).unwrap();
        assert_eq!(jittered, from);
    }

    #[test]
    fn find_missed_tasks_threshold_behavior() {
        // Created in the past, next fire already passed → missed.
        let t_missed = CronTask {
            id: "aaaaaaaa".into(),
            cron: "*/5 * * * *".into(),
            prompt: "p".into(),
            created_at: 1_000, // very old
            last_fired_at: None,
            recurring: true,
            permanent: false,
        };
        // Created just now — next fire after this moment → not missed.
        let now = now_ms();
        let t_fresh = CronTask {
            id: "bbbbbbbb".into(),
            cron: "*/5 * * * *".into(),
            prompt: "p".into(),
            created_at: now,
            last_fired_at: None,
            recurring: false,
            permanent: false,
        };

        let missed = find_missed_tasks(&[t_missed.clone(), t_fresh.clone()], now);
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0].id, t_missed.id);
    }

    #[test]
    fn is_recurring_task_aged_boundary() {
        let cfg = CronJitterConfig {
            recurring_max_age_ms: 10_000,
            ..DEFAULT_CRON_JITTER_CONFIG
        };
        let one_shot = CronTask {
            id: "1".into(),
            cron: "0 9 * * *".into(),
            prompt: "p".into(),
            created_at: 0,
            last_fired_at: None,
            recurring: false,
            permanent: false,
        };
        let recurring_fresh = CronTask {
            recurring: true,
            ..one_shot.clone()
        };
        let recurring_aged = CronTask {
            recurring: true,
            created_at: 0,
            ..one_shot.clone()
        };
        let permanent_old = CronTask {
            recurring: true,
            permanent: true,
            created_at: 0,
            ..one_shot.clone()
        };

        // One-shots never age out.
        assert!(!is_recurring_task_aged(&one_shot, 100_000, &cfg));
        // Within window.
        assert!(!is_recurring_task_aged(&recurring_fresh, 5_000, &cfg));
        // Just past the boundary.
        assert!(is_recurring_task_aged(&recurring_aged, 10_001, &cfg));
        // Permanent exempt.
        assert!(!is_recurring_task_aged(&permanent_old, 1_000_000, &cfg));
        // max_age_ms == 0 → unlimited.
        let unlimited = CronJitterConfig {
            recurring_max_age_ms: 0,
            ..cfg
        };
        assert!(!is_recurring_task_aged(
            &recurring_aged,
            i64::MAX / 2,
            &unlimited
        ));
    }

    #[test]
    fn path_helpers_match_expected_layout() {
        let root = PathBuf::from("/tmp/some-project");
        assert_eq!(cron_dir(&root), PathBuf::from("/tmp/some-project/.rebon"));
        assert_eq!(
            cron_file_path(&root),
            PathBuf::from("/tmp/some-project/.rebon/scheduled_tasks.json")
        );
    }
}
