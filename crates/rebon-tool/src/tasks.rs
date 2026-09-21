use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Task state that [`ToolContext`](crate::ToolContext) carries in its
/// extension bag.
///
/// `list_id` is snapshotted once at context-build time so every task tool in
/// a turn agrees even if `REBON_TEAM_NAME` changes mid-turn. Storage only —
/// read and written through the unchanged `task_runtime_controller()` /
/// `task_list_id()` accessors.
#[derive(Clone, Default)]
pub struct TaskContext {
    pub runtime_controller: Option<Arc<dyn crate::TaskRuntimeController>>,
    pub list_id: Option<String>,
}

const HIGH_WATER_MARK_FILE: &str = ".highwatermark";
const LIST_LOCK_FILE: &str = ".lock";
/// Retry budget for a contended lock: linear back-off from
/// `LOCK_RETRY_MIN_MS` up to `LOCK_RETRY_MAX_MS`, about nine seconds in
/// total. Thirty attempts (two seconds) was enough for two teammates but
/// not for a loaded machine, where sixteen writers churning one inbox
/// starved a caller past the budget and lost its write.
const LOCK_RETRIES: usize = 100;
const LOCK_RETRY_MIN_MS: u64 = 5;
const LOCK_RETRY_MAX_MS: u64 = 100;

static FALLBACK_TASK_LIST_ID: OnceLock<String> = OnceLock::new();
static TASK_LIST_NOTIFIERS: OnceLock<Mutex<HashMap<String, Weak<Notify>>>> = OnceLock::new();

fn task_list_notifiers() -> &'static Mutex<HashMap<String, Weak<Notify>>> {
    TASK_LIST_NOTIFIERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn task_list_notification(task_list_id: &str) -> Arc<Notify> {
    let key = sanitize_path_component(task_list_id);
    let mut notifiers = task_list_notifiers()
        .lock()
        .expect("task-list notifier registry poisoned");
    if let Some(notify) = notifiers.get(&key).and_then(Weak::upgrade) {
        return notify;
    }
    let notify = Arc::new(Notify::new());
    notifiers.insert(key, Arc::downgrade(&notify));
    notify
}

fn notify_task_list_changed(task_list_id: &str) {
    task_list_notification(task_list_id).notify_waiters();
}

/// Status of a stored task. Defined once in `rebon-types`; the serialized
/// literals (`pending` / `in_progress` / `completed`) are the on-disk shape of
/// every task file under the task-list directory.
pub use rebon_types::TaskListStatus;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    #[serde(
        rename = "activeForm",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub status: TaskListStatus,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(rename = "blockedBy", default)]
    pub blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// A stored task projected onto the row shape the list surfaces render.
///
/// The stored task carries fields no list surface reads — description, active
/// form, forward `blocks` edges, metadata. Dropping them here is the whole
/// difference between the two types, and doing it in one place keeps the TUI
/// from re-deriving the projection every frame.
impl From<&Task> for rebon_types::ListTask {
    fn from(task: &Task) -> Self {
        rebon_types::ListTask {
            id: task.id.clone(),
            subject: task.subject.clone(),
            status: task.status,
            owner: task.owner.clone(),
            blocked_by: task.blocked_by.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewTask {
    pub subject: String,
    pub description: String,
    pub active_form: Option<String>,
    pub owner: Option<String>,
    pub status: TaskListStatus,
    pub blocks: Vec<String>,
    pub blocked_by: Vec<String>,
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TaskPatch {
    pub subject: Option<String>,
    pub description: Option<String>,
    pub active_form: Option<Option<String>>,
    pub owner: Option<Option<String>>,
    pub status: Option<TaskListStatus>,
    pub blocks: Option<Vec<String>>,
    pub blocked_by: Option<Vec<String>>,
    pub metadata: Option<Option<Map<String, Value>>>,
}

/// Whether the Task system is active (replacing TodoWrite).
///
/// The name keeps the historical "v2" that marked this list as the successor
/// to the older TodoWrite tool. The environment is re-read on every call —
/// neither flag is cached.
/// Returns `true` for interactive sessions (the default for the TUI),
/// `false` for non-interactive/SDK mode. Can be force-enabled with
/// `REBON_ENABLE_TASKS=1`.
///
/// When this returns `true`, Task tools are available and TodoWrite is
/// disabled. When `false`, the inverse applies.
pub fn is_todo_v2_enabled() -> bool {
    if env_var("REBON_ENABLE_TASKS").is_some_and(|v| matches!(v.as_str(), "1" | "true")) {
        return true;
    }
    // rebon is TUI-only today, so interactive is the default.
    // If a non-interactive mode is added later, gate on that here.
    !env_var("REBON_NON_INTERACTIVE").is_some_and(|v| matches!(v.as_str(), "1" | "true"))
}

pub fn current_task_list_id() -> String {
    env_var("REBON_TASK_LIST_ID")
        .or_else(|| env_var("REBON_TEAM_NAME"))
        .or_else(|| env_var("REBON_SESSION_ID"))
        .unwrap_or_else(|| {
            FALLBACK_TASK_LIST_ID
                .get_or_init(generate_fallback_task_list_id)
                .clone()
        })
}

pub fn sanitize_path_component(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect()
}

/// `$REBON_CONFIG_DIR`, then `~/.rebon`, resolved the way the rest of the
/// process resolves it.
pub fn config_home_dir() -> PathBuf {
    rebon_session::default_config_home_dir()
}

pub fn tasks_dir(task_list_id: &str) -> PathBuf {
    config_home_dir()
        .join("tasks")
        .join(sanitize_path_component(task_list_id))
}

pub fn task_path(task_list_id: &str, task_id: &str) -> PathBuf {
    tasks_dir(task_list_id).join(format!("{}.json", sanitize_path_component(task_id)))
}

pub fn create_task(task_list_id: &str, input: NewTask) -> Result<String> {
    ensure_tasks_dir(task_list_id)?;
    let _lock = FileLock::acquire(&list_lock_path(task_list_id))?;

    let next_id = find_highest_task_id_unlocked(task_list_id)? + 1;
    let task = Task {
        id: next_id.to_string(),
        subject: input.subject,
        description: input.description,
        active_form: input.active_form,
        owner: input.owner,
        status: input.status,
        blocks: input.blocks,
        blocked_by: input.blocked_by,
        metadata: input.metadata,
    };
    write_task_unlocked(task_list_id, &task)?;
    notify_task_list_changed(task_list_id);
    Ok(task.id)
}

pub fn get_task(task_list_id: &str, task_id: &str) -> Result<Option<Task>> {
    read_task_file(&task_path(task_list_id, task_id))
}

pub fn list_tasks(task_list_id: &str) -> Result<Vec<Task>> {
    list_tasks_unlocked(task_list_id)
}

pub fn update_task(task_list_id: &str, task_id: &str, patch: TaskPatch) -> Result<Option<Task>> {
    ensure_tasks_dir(task_list_id)?;
    let _lock = FileLock::acquire(&list_lock_path(task_list_id))?;
    let updated = update_task_unlocked(task_list_id, task_id, patch)?;
    if updated.is_some() {
        notify_task_list_changed(task_list_id);
    }
    Ok(updated)
}

pub fn cleanup_in_progress_tasks_for_agent(
    task_list_id: &str,
    agent_id: &str,
    status: TaskListStatus,
) -> Result<usize> {
    ensure_tasks_dir(task_list_id)?;
    let _lock = FileLock::acquire(&list_lock_path(task_list_id))?;

    let mut updated = 0;
    let agent_name = agent_id.split('@').next().unwrap_or(agent_id);
    for mut task in list_tasks_unlocked(task_list_id)? {
        let belongs_to_agent = task_agent_id(&task).as_deref() == Some(agent_id)
            || task.owner.as_deref() == Some(agent_id)
            || task.owner.as_deref() == Some(agent_name);
        if task.status != TaskListStatus::InProgress || !belongs_to_agent {
            continue;
        }
        task.status = status.clone();
        task.owner = None;
        write_task_unlocked(task_list_id, &task)?;
        updated += 1;
    }
    if updated > 0 {
        notify_task_list_changed(task_list_id);
    }
    Ok(updated)
}

pub fn cleanup_in_progress_tasks_by_ids(
    task_list_id: &str,
    task_ids: &[String],
    status: TaskListStatus,
) -> Result<usize> {
    ensure_tasks_dir(task_list_id)?;
    let _lock = FileLock::acquire(&list_lock_path(task_list_id))?;

    let mut updated = 0;
    for task_id in task_ids {
        let Some(mut task) = get_task_unlocked(task_list_id, task_id)? else {
            continue;
        };
        if task.status != TaskListStatus::InProgress {
            continue;
        }
        task.status = status.clone();
        task.owner = None;
        write_task_unlocked(task_list_id, &task)?;
        updated += 1;
    }
    if updated > 0 {
        notify_task_list_changed(task_list_id);
    }
    Ok(updated)
}

pub fn delete_task(task_list_id: &str, task_id: &str) -> Result<bool> {
    ensure_tasks_dir(task_list_id)?;
    let _lock = FileLock::acquire(&list_lock_path(task_list_id))?;

    let path = task_path(task_list_id, task_id);
    if !path.exists() {
        return Ok(false);
    }

    if let Ok(parsed_id) = task_id.parse::<u64>() {
        let current_mark = read_high_water_mark(task_list_id)?;
        if parsed_id > current_mark {
            write_high_water_mark(task_list_id, parsed_id)?;
        }
    }

    fs::remove_file(&path)
        .with_context(|| format!("failed to delete task file {}", path.display()))?;
    cleanup_task_references_unlocked(task_list_id, task_id)?;
    notify_task_list_changed(task_list_id);
    Ok(true)
}

/// Delete all task files and update the high-water mark so IDs are
/// never reused. The task-list lock is held for the whole sweep, so a
/// concurrent create cannot slip a fresh id in between the two steps.
pub fn reset_task_list(task_list_id: &str) -> Result<()> {
    ensure_tasks_dir(task_list_id)?;
    let _lock = FileLock::acquire(&list_lock_path(task_list_id))?;

    // Persist the highest id so new tasks start after it.
    let highest = find_highest_task_id_unlocked(task_list_id)?;
    if highest > 0 {
        let existing = read_high_water_mark(task_list_id)?;
        if highest > existing {
            write_high_water_mark(task_list_id, highest)?;
        }
    }

    // Remove every .json task file.
    for path in list_task_paths(task_list_id)? {
        let _ = fs::remove_file(&path);
    }
    notify_task_list_changed(task_list_id);
    Ok(())
}

pub fn block_task(task_list_id: &str, from_task_id: &str, to_task_id: &str) -> Result<bool> {
    ensure_tasks_dir(task_list_id)?;
    let _lock = FileLock::acquire(&list_lock_path(task_list_id))?;

    let Some(mut from_task) = get_task_unlocked(task_list_id, from_task_id)? else {
        return Ok(false);
    };
    let Some(mut to_task) = get_task_unlocked(task_list_id, to_task_id)? else {
        return Ok(false);
    };

    let mut changed = false;
    if !from_task.blocks.iter().any(|id| id == to_task_id) {
        from_task.blocks.push(to_task_id.to_string());
        changed = true;
    }
    if !to_task.blocked_by.iter().any(|id| id == from_task_id) {
        to_task.blocked_by.push(from_task_id.to_string());
        changed = true;
    }

    if changed {
        write_task_unlocked(task_list_id, &from_task)?;
        write_task_unlocked(task_list_id, &to_task)?;
        notify_task_list_changed(task_list_id);
    }

    Ok(true)
}

pub fn is_internal_task(task: &Task) -> bool {
    task.metadata
        .as_ref()
        .and_then(|metadata| metadata.get("_internal"))
        .is_some_and(|value| value == &Value::Bool(true))
}

fn task_agent_id(task: &Task) -> Option<String> {
    task.metadata
        .as_ref()
        .and_then(|metadata| {
            metadata
                .get("agent_id")
                .or_else(|| metadata.get("agentId"))
                .or_else(|| metadata.get("parent_agent_id"))
                .or_else(|| metadata.get("parentAgentId"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|agent_id| !agent_id.is_empty())
        .map(str::to_string)
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn generate_fallback_task_list_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("sess-{}-{nanos:x}", std::process::id())
}

fn ensure_tasks_dir(task_list_id: &str) -> Result<()> {
    let dir = tasks_dir(task_list_id);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create tasks dir {}", dir.display()))
}

fn list_lock_path(task_list_id: &str) -> PathBuf {
    tasks_dir(task_list_id).join(LIST_LOCK_FILE)
}

fn high_water_mark_path(task_list_id: &str) -> PathBuf {
    tasks_dir(task_list_id).join(HIGH_WATER_MARK_FILE)
}

fn read_high_water_mark(task_list_id: &str) -> Result<u64> {
    let path = high_water_mark_path(task_list_id);
    match fs::read_to_string(&path) {
        Ok(content) => Ok(content.trim().parse::<u64>().unwrap_or(0)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => {
            Err(err).with_context(|| format!("failed to read high-water mark {}", path.display()))
        }
    }
}

fn write_high_water_mark(task_list_id: &str, value: u64) -> Result<()> {
    let path = high_water_mark_path(task_list_id);
    fs::write(&path, value.to_string())
        .with_context(|| format!("failed to write high-water mark {}", path.display()))
}

fn find_highest_task_id_unlocked(task_list_id: &str) -> Result<u64> {
    let from_files = list_task_paths(task_list_id)?
        .into_iter()
        .filter_map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .filter_map(|stem| stem.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    Ok(from_files.max(read_high_water_mark(task_list_id)?))
}

fn list_task_paths(task_list_id: &str) -> Result<Vec<PathBuf>> {
    let dir = tasks_dir(task_list_id);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to list tasks dir {}", dir.display()))
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn list_tasks_unlocked(task_list_id: &str) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    for path in list_task_paths(task_list_id)? {
        if let Some(task) = read_task_file(&path)? {
            tasks.push(task);
        }
    }
    tasks.sort_by(|left, right| compare_task_ids(&left.id, &right.id));
    Ok(tasks)
}

fn compare_task_ids(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left_num), Ok(right_num)) => left_num.cmp(&right_num),
        _ => left.cmp(right),
    }
}

fn read_task_file(path: &Path) -> Result<Option<Task>> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let task = serde_json::from_str::<Task>(&content)
                .with_context(|| format!("failed to parse task file {}", path.display()))?;
            Ok(Some(task))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("failed to read task file {}", path.display()))
        }
    }
}

fn get_task_unlocked(task_list_id: &str, task_id: &str) -> Result<Option<Task>> {
    read_task_file(&task_path(task_list_id, task_id))
}

fn write_task_unlocked(task_list_id: &str, task: &Task) -> Result<()> {
    ensure_tasks_dir(task_list_id)?;
    let path = task_path(task_list_id, &task.id);
    let bytes = serde_json::to_vec_pretty(task)
        .with_context(|| format!("failed to serialize task {}", task.id))?;
    fs::write(&path, bytes).with_context(|| format!("failed to write task file {}", path.display()))
}

fn update_task_unlocked(
    task_list_id: &str,
    task_id: &str,
    patch: TaskPatch,
) -> Result<Option<Task>> {
    let Some(mut task) = get_task_unlocked(task_list_id, task_id)? else {
        return Ok(None);
    };

    if let Some(subject) = patch.subject {
        task.subject = subject;
    }
    if let Some(description) = patch.description {
        task.description = description;
    }
    if let Some(active_form) = patch.active_form {
        task.active_form = active_form;
    }
    if let Some(owner) = patch.owner {
        task.owner = owner;
    }
    if let Some(status) = patch.status {
        task.status = status;
        if task.status != TaskListStatus::InProgress {
            task.owner = None;
        }
    }
    if let Some(blocks) = patch.blocks {
        task.blocks = blocks;
    }
    if let Some(blocked_by) = patch.blocked_by {
        task.blocked_by = blocked_by;
    }
    if let Some(metadata) = patch.metadata {
        task.metadata = metadata;
    }

    write_task_unlocked(task_list_id, &task)?;
    Ok(Some(task))
}

fn cleanup_task_references_unlocked(task_list_id: &str, deleted_task_id: &str) -> Result<()> {
    for mut task in list_tasks_unlocked(task_list_id)? {
        let original_blocks_len = task.blocks.len();
        let original_blocked_by_len = task.blocked_by.len();
        task.blocks.retain(|task_id| task_id != deleted_task_id);
        task.blocked_by.retain(|task_id| task_id != deleted_task_id);

        if task.blocks.len() != original_blocks_len
            || task.blocked_by.len() != original_blocked_by_len
        {
            write_task_unlocked(task_list_id, &task)?;
        }
    }
    Ok(())
}

pub(crate) struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("lock path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create lock dir {}", parent.display()))?;

        let mut contention: Option<std::io::Error> = None;
        for attempt in 0..=LOCK_RETRIES {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(_) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                    })
                }
                Err(err) if is_lock_contention(&err) && attempt < LOCK_RETRIES => {
                    contention = Some(err);
                    let delay_ms = (LOCK_RETRY_MIN_MS + attempt as u64 * LOCK_RETRY_MIN_MS)
                        .min(LOCK_RETRY_MAX_MS);
                    thread::sleep(Duration::from_millis(delay_ms));
                }
                Err(err) if is_lock_contention(&err) => {
                    contention = Some(err);
                    break;
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to acquire lock {}", path.display()))
                }
            }
        }

        match contention {
            // An exhausted `AlreadyExists` says all there is to say; the
            // Windows codes below do not, and a lock dir that is genuinely
            // unwritable also lands here, so keep the OS error attached.
            Some(err) if err.kind() != std::io::ErrorKind::AlreadyExists => {
                Err(err).with_context(|| format!("failed to acquire lock {}", path.display()))
            }
            _ => Err(anyhow!("failed to acquire lock {}", path.display())),
        }
    }
}

/// Whether a failed `create_new` means *someone else holds it right now*
/// rather than *this can never work*.
///
/// `AlreadyExists` is the ordinary answer. Windows has two more, and losing
/// them cost a real bug: `write_mailbox_message` gave up outright whenever two
/// teammates wrote to one inbox at the same moment.
fn is_lock_contention(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::AlreadyExists || is_windows_lock_contention(err)
}

/// The two Windows-only ways a lock file refuses an opener that is not
/// actually competing for a *held* lock:
///
/// * `ERROR_ACCESS_DENIED` — the previous holder's `remove_file` marked the
///   file delete-pending. Windows keeps the directory entry until the last
///   handle closes, and every `CreateFile` in that window is refused. The
///   holder is on its way out, so this is contention wearing another error
///   code; it is also the *most likely* code to see under a tight
///   release/re-acquire churn, not the least.
/// * `ERROR_SHARING_VIOLATION` / `ERROR_LOCK_VIOLATION` — a scanner or
///   indexer has the file open for the moment.
///
/// Matching raw codes rather than `ErrorKind` keeps a genuinely unwritable
/// directory (also `PermissionDenied`) from being retried on a whim — it will
/// still be retried when it reports code 5, but the retry budget is ~1.5s and
/// the OS error survives into the final message.
#[cfg(windows)]
fn is_windows_lock_contention(err: &std::io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    matches!(
        err.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

#[cfg(not(windows))]
fn is_windows_lock_contention(_err: &std::io::Error) -> bool {
    false
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    pub struct TestConfigHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
        task_list_id: String,
        old_config_dir: Option<String>,
        old_task_list_id: Option<String>,
        old_team_name: Option<String>,
        old_session_id: Option<String>,
    }

    impl TestConfigHome {
        pub fn new(prefix: &str) -> Self {
            let guard = lock_env();
            let nonce = TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            let dir = tempfile::Builder::new()
                .prefix(&format!("rebon-task-tests-{prefix}-"))
                .tempdir()
                .unwrap();

            let task_list_id = format!("{prefix}-{nonce}");
            let old_config_dir = std::env::var("REBON_CONFIG_DIR").ok();
            let old_task_list_id = std::env::var("REBON_TASK_LIST_ID").ok();
            let old_team_name = std::env::var("REBON_TEAM_NAME").ok();
            let old_session_id = std::env::var("REBON_SESSION_ID").ok();

            std::env::set_var("REBON_CONFIG_DIR", dir.path());
            std::env::set_var("REBON_TASK_LIST_ID", &task_list_id);
            std::env::remove_var("REBON_TEAM_NAME");
            std::env::remove_var("REBON_SESSION_ID");

            Self {
                _guard: guard,
                dir,
                task_list_id,
                old_config_dir,
                old_task_list_id,
                old_team_name,
                old_session_id,
            }
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        pub fn task_list_id(&self) -> &str {
            &self.task_list_id
        }
    }

    impl Drop for TestConfigHome {
        fn drop(&mut self) {
            restore_env("REBON_CONFIG_DIR", self.old_config_dir.as_deref());
            restore_env("REBON_TASK_LIST_ID", self.old_task_list_id.as_deref());
            restore_env("REBON_TEAM_NAME", self.old_team_name.as_deref());
            restore_env("REBON_SESSION_ID", self.old_session_id.as_deref());
        }
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        crate::env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn restore_env(name: &str, value: Option<&str>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::test_support::TestConfigHome;
    use serde_json::json;
    use std::sync::{Arc, Barrier};

    /// Releasing a lock and re-taking it is the hot path whenever several
    /// writers share one file, and on Windows that hands `create_new` an
    /// error it does not otherwise see: a removed file stays in its
    /// directory, marked delete-pending, until the last handle closes, and
    /// an open that lands in that window is refused with ACCESS_DENIED
    /// rather than ALREADY_EXISTS. It is contention all the same, so
    /// acquiring has to ride it out instead of giving up.
    #[test]
    fn concurrent_lock_acquire_rides_out_contention() {
        let dir = tempfile::Builder::new()
            .prefix("rebon-file-lock-contention-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("inbox.json.lock");
        let threads = 16;
        let rounds = 40;
        let barrier = Arc::new(Barrier::new(threads));

        let handles = (0..threads)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..rounds {
                        // Dropped at the end of the statement: the tightest
                        // release/re-acquire churn the callers can produce.
                        FileLock::acquire(&path).expect("contention must not fail an acquire");
                    }
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }
    }

    fn sample_task(subject: &str) -> NewTask {
        NewTask {
            subject: subject.to_string(),
            description: format!("{subject} desc"),
            active_form: None,
            owner: None,
            status: TaskListStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: None,
        }
    }

    #[test]
    fn sanitize_path_component_replaces_unsafe_characters() {
        assert_eq!(sanitize_path_component("abc/def ghi"), "abc-def-ghi");
        assert_eq!(sanitize_path_component("A_B-9"), "A_B-9");
    }

    #[test]
    fn create_delete_and_recreate_task_uses_high_water_mark() {
        let home = TestConfigHome::new("high-water");
        let first = create_task(home.task_list_id(), sample_task("first")).unwrap();
        assert_eq!(first, "1");

        assert!(delete_task(home.task_list_id(), &first).unwrap());

        let second = create_task(home.task_list_id(), sample_task("second")).unwrap();
        assert_eq!(second, "2");
    }

    #[test]
    fn update_task_applies_patch_fields() {
        let home = TestConfigHome::new("update");
        let task_id = create_task(home.task_list_id(), sample_task("first")).unwrap();

        let updated = update_task(
            home.task_list_id(),
            &task_id,
            TaskPatch {
                subject: Some("renamed".into()),
                description: Some("updated desc".into()),
                active_form: Some(Some("Running rename".into())),
                owner: Some(Some("agent-a".into())),
                status: Some(TaskListStatus::InProgress),
                ..TaskPatch::default()
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(updated.subject, "renamed");
        assert_eq!(updated.description, "updated desc");
        assert_eq!(updated.active_form.as_deref(), Some("Running rename"));
        assert_eq!(updated.owner.as_deref(), Some("agent-a"));
        assert_eq!(updated.status, TaskListStatus::InProgress);
    }

    #[test]
    fn completing_or_requeueing_task_releases_owner() {
        let home = TestConfigHome::new("owner-release");
        let task_id = create_task(
            home.task_list_id(),
            NewTask {
                owner: Some("agent-a".into()),
                status: TaskListStatus::InProgress,
                ..sample_task("owned")
            },
        )
        .unwrap();

        let completed = update_task(
            home.task_list_id(),
            &task_id,
            TaskPatch {
                status: Some(TaskListStatus::Completed),
                ..TaskPatch::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(completed.owner, None);

        let requeued = update_task(
            home.task_list_id(),
            &task_id,
            TaskPatch {
                owner: Some(Some("agent-b".into())),
                status: Some(TaskListStatus::Pending),
                ..TaskPatch::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(requeued.owner, None);
    }

    #[test]
    fn block_task_links_both_sides() {
        let home = TestConfigHome::new("block");
        let task_a = create_task(home.task_list_id(), sample_task("a")).unwrap();
        let task_b = create_task(home.task_list_id(), sample_task("b")).unwrap();

        assert!(block_task(home.task_list_id(), &task_a, &task_b).unwrap());

        let a = get_task(home.task_list_id(), &task_a).unwrap().unwrap();
        let b = get_task(home.task_list_id(), &task_b).unwrap().unwrap();
        assert_eq!(a.blocks, vec![task_b.clone()]);
        assert_eq!(b.blocked_by, vec![task_a.clone()]);
    }

    #[test]
    fn delete_task_cleans_up_block_references() {
        let home = TestConfigHome::new("cleanup");
        let task_a = create_task(home.task_list_id(), sample_task("a")).unwrap();
        let task_b = create_task(home.task_list_id(), sample_task("b")).unwrap();
        block_task(home.task_list_id(), &task_a, &task_b).unwrap();

        assert!(delete_task(home.task_list_id(), &task_a).unwrap());

        let b = get_task(home.task_list_id(), &task_b).unwrap().unwrap();
        assert!(b.blocked_by.is_empty());
    }

    #[test]
    fn list_tasks_sorts_numeric_ids_and_preserves_metadata() {
        let home = TestConfigHome::new("list");
        let mut metadata = Map::new();
        metadata.insert("_internal".into(), json!(true));

        create_task(
            home.task_list_id(),
            NewTask {
                subject: "internal".into(),
                description: "internal desc".into(),
                active_form: None,
                owner: None,
                status: TaskListStatus::Pending,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                metadata: Some(metadata),
            },
        )
        .unwrap();
        create_task(home.task_list_id(), sample_task("visible")).unwrap();

        let tasks = list_tasks(home.task_list_id()).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(is_internal_task(&tasks[0]));
        assert_eq!(tasks[1].id, "2");
        assert!(home.path().join("tasks").exists());
    }

    #[test]
    fn cleanup_in_progress_tasks_for_agent_marks_only_matching_tasks() {
        let home = TestConfigHome::new("agent-cleanup");
        let mut agent_metadata = Map::new();
        agent_metadata.insert("agent_id".into(), json!("agent-a"));
        let mut other_metadata = Map::new();
        other_metadata.insert("agent_id".into(), json!("agent-b"));

        let matching = create_task(
            home.task_list_id(),
            NewTask {
                owner: Some("agent-a".into()),
                status: TaskListStatus::InProgress,
                metadata: Some(agent_metadata),
                ..sample_task("matching")
            },
        )
        .unwrap();
        let other_agent = create_task(
            home.task_list_id(),
            NewTask {
                status: TaskListStatus::InProgress,
                metadata: Some(other_metadata),
                ..sample_task("other-agent")
            },
        )
        .unwrap();
        let pending = create_task(
            home.task_list_id(),
            NewTask {
                metadata: Some({
                    let mut metadata = Map::new();
                    metadata.insert("agent_id".into(), json!("agent-a"));
                    metadata
                }),
                ..sample_task("pending")
            },
        )
        .unwrap();

        let updated = cleanup_in_progress_tasks_for_agent(
            home.task_list_id(),
            "agent-a",
            TaskListStatus::Pending,
        )
        .unwrap();

        assert_eq!(updated, 1);
        let matching = get_task(home.task_list_id(), &matching).unwrap().unwrap();
        assert_eq!(matching.status, TaskListStatus::Pending);
        assert_eq!(matching.owner, None);
        assert_eq!(
            get_task(home.task_list_id(), &other_agent)
                .unwrap()
                .unwrap()
                .status,
            TaskListStatus::InProgress
        );
        assert_eq!(
            get_task(home.task_list_id(), &pending)
                .unwrap()
                .unwrap()
                .status,
            TaskListStatus::Pending
        );
    }

    #[test]
    fn cleanup_in_progress_tasks_by_ids_marks_only_explicit_active_tasks() {
        let home = TestConfigHome::new("linked-task-cleanup");
        let matching = create_task(
            home.task_list_id(),
            NewTask {
                owner: Some("delegated-worker".into()),
                status: TaskListStatus::InProgress,
                ..sample_task("matching")
            },
        )
        .unwrap();
        let untouched = create_task(
            home.task_list_id(),
            NewTask {
                status: TaskListStatus::InProgress,
                ..sample_task("untouched")
            },
        )
        .unwrap();
        let already_done = create_task(
            home.task_list_id(),
            NewTask {
                status: TaskListStatus::Completed,
                ..sample_task("already-done")
            },
        )
        .unwrap();

        let updated = cleanup_in_progress_tasks_by_ids(
            home.task_list_id(),
            &[matching.clone(), already_done.clone(), "missing".into()],
            TaskListStatus::Completed,
        )
        .unwrap();

        assert_eq!(updated, 1);
        let matching = get_task(home.task_list_id(), &matching).unwrap().unwrap();
        assert_eq!(matching.status, TaskListStatus::Completed);
        assert_eq!(matching.owner, None);
        assert_eq!(
            get_task(home.task_list_id(), &untouched)
                .unwrap()
                .unwrap()
                .status,
            TaskListStatus::InProgress
        );
        assert_eq!(
            get_task(home.task_list_id(), &already_done)
                .unwrap()
                .unwrap()
                .status,
            TaskListStatus::Completed
        );
    }

    #[test]
    fn reset_task_list_clears_all_tasks_and_preserves_high_water_mark() {
        let home = TestConfigHome::new("reset");
        let id1 = create_task(home.task_list_id(), sample_task("a")).unwrap();
        let id2 = create_task(home.task_list_id(), sample_task("b")).unwrap();
        let id3 = create_task(home.task_list_id(), sample_task("c")).unwrap();
        assert_eq!(list_tasks(home.task_list_id()).unwrap().len(), 3);

        reset_task_list(home.task_list_id()).unwrap();

        // All tasks gone.
        assert!(list_tasks(home.task_list_id()).unwrap().is_empty());

        // High-water mark preserved — next task id continues after the max.
        let max_id = [&id1, &id2, &id3]
            .iter()
            .filter_map(|s| s.parse::<u64>().ok())
            .max()
            .unwrap();
        let next = create_task(home.task_list_id(), sample_task("after-reset")).unwrap();
        assert_eq!(next.parse::<u64>().unwrap(), max_id + 1);
    }
}
