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

#[derive(Debug, Clone, Default)]
pub struct SessionCronStore {
    tasks: Arc<Mutex<Vec<SessionCronTask>>>,
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
