//! Cron scheduler — the tokio task that drives fires.
//!
//! Implementation for the cron scheduler. Each rebon process starts one
//! scheduler per project root. The first process wins the owner lock and
//! becomes the active scheduler; later ones fall into a probe loop and take
//! over if the owner dies.
//!
//! Responsibilities:
//!
//! - **Owner election**: `fs2::try_lock_exclusive` on `.scheduler.lock`.
//! - **Missed-task surfacing**: on first acquisition, compute which persisted
//!   tasks have fire times in the past; drain them into the poller with a
//!   single "tasks you missed while away" notice, then delete/mark.
//! - **Tick loop**: every 1s, compare `now_ms()` against each task's
//!   jittered next-fire; `poller.enqueue(prompt)` on hit.
//! - **Recurring lifecycle**: after fire, either
//!   - recompute next-fire with jitter (mark `last_fired_at`), or
//!   - delete when aged past `recurring_max_age_ms`.
//! - **One-shot lifecycle**: delete after fire.
//! - **File watch**: `notify` picks up external writes (CronCreate /
//!   CronDelete from another process, or manual edits) and reloads the
//!   in-memory task set.
//!
//! ## Design notes
//!
//! - Time source: `rebon_tool::cron::tasks::now_ms()`; local-tz math
//!   happens inside the cron arithmetic only.
//! - `notify` debounce: Windows atomic-rename triggers multiple events; we
//!   collapse them with a 300ms sleep and re-read.
//! - `SchedulerHandle::stop()` aborts the task and releases the lock. The
//!   kernel releases the `fs2` lock on process crash, so graceful shutdown
//!   is a nice-to-have rather than a correctness requirement.

use crate::cron::lock::{try_acquire_scheduler_lock, SchedulerLock};
use crate::cron::poller::CronPoller;
use rebon_tool::cron::tasks::{
    cron_dir, cron_disabled, cron_file_path, find_missed_tasks, is_recurring_task_aged,
    jittered_next_cron_run_ms, mark_cron_tasks_fired, next_cron_run_ms, now_ms,
    one_shot_jittered_next_cron_run_ms, read_cron_tasks, remove_cron_tasks, CronJitterConfig,
    CronTask, SessionCronStore, SessionCronTask, DEFAULT_CRON_JITTER_CONFIG,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const FILE_STABILITY_MS: u64 = 300;

/// Handle returned by [`start_scheduler`]. Dropping it (or calling
/// [`SchedulerHandle::stop`]) aborts the loop and releases the owner lock.
pub struct SchedulerHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl SchedulerHandle {
    /// Request shutdown and wait for the scheduler task to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(()).await;
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.try_send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

/// Configuration knobs exposed to callers (mostly for tests).
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Project root containing `.rebon/scheduled_tasks.json`.
    pub project_root: PathBuf,
    /// How often the loop compares the clock against each task's next
    /// fire time. Defaults to 1 second.
    pub tick: Duration,
    /// Owner probe interval — how often non-owners re-try acquisition.
    pub probe: Duration,
    /// Jitter configuration.
    pub jitter: CronJitterConfig,
}

impl SchedulerConfig {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root,
            tick: Duration::from_secs(1),
            probe: Duration::from_secs(5),
            jitter: DEFAULT_CRON_JITTER_CONFIG,
        }
    }
}

/// Spawn the scheduler task. Returns a [`SchedulerHandle`]; when it drops,
/// the loop stops. Safe to call from non-async contexts as long as a tokio
/// runtime is currently entered (typical for rebon wiring).
pub fn start_scheduler(config: SchedulerConfig, poller: Arc<CronPoller>) -> SchedulerHandle {
    start_scheduler_with_session_store(config, poller, None)
}

pub fn start_scheduler_with_session_store(
    config: SchedulerConfig,
    poller: Arc<CronPoller>,
    session_store: Option<Arc<SessionCronStore>>,
) -> SchedulerHandle {
    let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
    let join = tokio::spawn(run_scheduler(config, poller, session_store, stop_rx));
    SchedulerHandle {
        stop_tx: Some(stop_tx),
        join: Some(join),
    }
}

async fn run_scheduler(
    config: SchedulerConfig,
    poller: Arc<CronPoller>,
    session_store: Option<Arc<SessionCronStore>>,
    mut stop_rx: mpsc::Receiver<()>,
) {
    if cron_disabled() {
        tracing::debug!("[CronScheduler] disabled by REBON_DISABLE_CRON");
        return;
    }

    let rebon_dir = cron_dir(&config.project_root);
    if let Err(err) = std::fs::create_dir_all(&rebon_dir) {
        tracing::warn!(
            dir = %rebon_dir.display(),
            error = %err,
            "[CronScheduler] failed to create .rebon dir — scheduler will not run"
        );
        return;
    }

    let (reload_tx, mut reload_rx) = mpsc::channel::<()>(1);
    let watcher = spawn_file_watcher(&config.project_root, reload_tx);
    let _watcher = watcher;

    let mut owner: Option<SchedulerLock> = None;
    let mut waiting_logged = false;
    let mut tasks: Vec<CronTask> = Vec::new();
    let mut next_fire: HashMap<String, i64> = HashMap::new();

    let mut tick_interval = tokio::time::interval(config.tick);
    tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut probe_interval = tokio::time::interval(config.probe);
    probe_interval.tick().await;

    try_acquire_owner(
        &config,
        &rebon_dir,
        &poller,
        &mut owner,
        &mut waiting_logged,
        &mut tasks,
        &mut next_fire,
    );

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                if cron_disabled() {
                    continue;
                }
                let now = now_ms();
                if let Some(store) = &session_store {
                    run_session_tick(&config, &poller, store, now);
                }
                if owner.is_some() {
                    run_tick(
                        &config,
                        &poller,
                        &mut tasks,
                        &mut next_fire,
                        now,
                    );
                }
            }
            _ = probe_interval.tick() => {
                if cron_disabled() || owner.is_some() {
                    continue;
                }
                try_acquire_owner(
                    &config,
                    &rebon_dir,
                    &poller,
                    &mut owner,
                    &mut waiting_logged,
                    &mut tasks,
                    &mut next_fire,
                );
            }
            _ = reload_rx.recv() => {
                if cron_disabled() || owner.is_none() {
                    continue;
                }
                tracing::debug!("[CronScheduler] reload triggered by file watcher");
                tasks = read_cron_tasks(&config.project_root);
                next_fire = compute_next_fire_table(&tasks, now_ms(), &config.jitter);
            }
            _ = stop_rx.recv() => {
                tracing::debug!("[CronScheduler] stop requested");
                return;
            }
        }
    }
}

fn try_acquire_owner(
    config: &SchedulerConfig,
    rebon_dir: &PathBuf,
    poller: &Arc<CronPoller>,
    owner: &mut Option<SchedulerLock>,
    waiting_logged: &mut bool,
    tasks: &mut Vec<CronTask>,
    next_fire: &mut HashMap<String, i64>,
) {
    match try_acquire_scheduler_lock(rebon_dir) {
        Ok(Some(held)) => {
            if *waiting_logged {
                tracing::debug!(
                    dir = %rebon_dir.display(),
                    "[CronScheduler] owner lock acquired after waiting"
                );
            }
            surface_missed_tasks(&config.project_root, poller);
            *tasks = read_cron_tasks(&config.project_root);
            *next_fire = compute_next_fire_table(tasks, now_ms(), &config.jitter);
            *owner = Some(held);
        }
        Ok(None) => {
            if !*waiting_logged {
                tracing::debug!(
                    dir = %rebon_dir.display(),
                    probe_secs = config.probe.as_secs(),
                    "[CronScheduler] owner lock held by another process — waiting"
                );
                *waiting_logged = true;
            } else {
                tracing::trace!(
                    dir = %rebon_dir.display(),
                    "[CronScheduler] still waiting on owner lock"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "[CronScheduler] lock acquire failed — retrying"
            );
        }
    }
}

/// Surface tasks whose fire time passed while rebon was offline. Only missed
/// durable one-shots are surfaced; recurring tasks are left for normal ticks.
fn surface_missed_tasks(project_root: &PathBuf, poller: &Arc<CronPoller>) {
    let tasks = read_cron_tasks(project_root);
    let now = now_ms();
    let missed: Vec<CronTask> = find_missed_tasks(&tasks, now)
        .into_iter()
        .filter(|task| !task.recurring)
        .collect();
    if missed.is_empty() {
        return;
    }

    poller.enqueue(build_missed_task_notification(&missed));

    let ids: Vec<String> = missed.iter().map(|t| t.id.clone()).collect();
    if let Err(err) = remove_cron_tasks(project_root, &ids) {
        tracing::warn!(
            error = %err,
            "[CronScheduler] failed to prune missed one-shots"
        );
    }
}

fn build_missed_task_notification(missed: &[CronTask]) -> String {
    let mut out = String::from(
        "The following scheduled one-shot tasks were missed while rebon was not running.\n\n\
Do not directly execute these prompts. First call AskUserQuestion to ask the user whether each missed task should run now, be rescheduled, or be discarded.\n",
    );
    for task in missed {
        let fence = code_fence_for(&task.prompt);
        out.push_str(&format!(
            "\nTask {} ({}) missed prompt:\n{fence}\n{}\n{fence}\n",
            task.id, task.cron, task.prompt
        ));
    }
    out.push_str(
        "\nThese missed one-shot tasks have been removed from .rebon/scheduled_tasks.json.",
    );
    out
}

fn code_fence_for(prompt: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in prompt.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn compute_next_fire_table(
    tasks: &[CronTask],
    _from_ms: i64,
    cfg: &CronJitterConfig,
) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    for task in tasks {
        let next = if task.recurring {
            let anchor = task.last_fired_at.unwrap_or(task.created_at);
            jittered_next_cron_run_ms(&task.cron, anchor, &task.id, cfg)
        } else {
            one_shot_jittered_next_cron_run_ms(&task.cron, task.created_at, &task.id, cfg)
        };
        if let Some(t) = next {
            out.insert(task.id.clone(), t);
        }
    }
    out
}

/// One scheduler tick. Factored out of the async loop for deterministic
/// testing with a fixed clock.
fn run_tick(
    config: &SchedulerConfig,
    poller: &Arc<CronPoller>,
    tasks: &mut Vec<CronTask>,
    next_fire: &mut HashMap<String, i64>,
    now: i64,
) {
    let mut fired_ids: Vec<String> = Vec::new();
    let mut to_remove: Vec<String> = Vec::new();

    for task in tasks.iter() {
        let Some(&fire_at) = next_fire.get(&task.id) else {
            continue;
        };
        if now < fire_at {
            continue;
        }

        // Fire the prompt.
        poller.enqueue(task.prompt.clone());

        if !task.recurring {
            to_remove.push(task.id.clone());
            continue;
        }

        // Recurring: age out, or schedule next fire.
        if is_recurring_task_aged(task, now, &config.jitter) {
            to_remove.push(task.id.clone());
            continue;
        }

        if let Some(next) = jittered_next_cron_run_ms(&task.cron, now, &task.id, &config.jitter) {
            next_fire.insert(task.id.clone(), next);
            fired_ids.push(task.id.clone());
        } else {
            // Couldn't compute next — drop so we don't loop firing it.
            to_remove.push(task.id.clone());
        }
    }

    if !fired_ids.is_empty() {
        if let Err(err) = mark_cron_tasks_fired(&config.project_root, &fired_ids, now) {
            tracing::warn!(
                error = %err,
                "[CronScheduler] failed to stamp last_fired_at"
            );
        }
    }

    if !to_remove.is_empty() {
        if let Err(err) = remove_cron_tasks(&config.project_root, &to_remove) {
            tracing::warn!(
                error = %err,
                "[CronScheduler] failed to prune completed tasks"
            );
        }
        tasks.retain(|t| !to_remove.contains(&t.id));
        for id in &to_remove {
            next_fire.remove(id);
        }
    }

    // Note: `tasks` may be stale if an external write landed mid-tick. The
    // file watcher delivers a reload event on the next iteration and we
    // rebuild from disk then.
    let _ = next_cron_run_ms; // silence unused-import when tests strip paths
}

fn run_session_tick(
    config: &SchedulerConfig,
    poller: &Arc<CronPoller>,
    store: &Arc<SessionCronStore>,
    now: i64,
) {
    let mut tasks = store.list();
    let mut next_fire = compute_session_next_fire_table(&tasks, now, &config.jitter);
    run_session_tick_inner(config, poller, store, &mut tasks, &mut next_fire, now);
}

fn compute_session_next_fire_table(
    tasks: &[SessionCronTask],
    from_ms: i64,
    cfg: &CronJitterConfig,
) -> HashMap<String, i64> {
    let cron_tasks: Vec<CronTask> = tasks.iter().map(SessionCronTask::to_cron_task).collect();
    compute_next_fire_table(&cron_tasks, from_ms, cfg)
}

fn run_session_tick_inner(
    config: &SchedulerConfig,
    poller: &Arc<CronPoller>,
    store: &Arc<SessionCronStore>,
    tasks: &mut Vec<SessionCronTask>,
    next_fire: &mut HashMap<String, i64>,
    now: i64,
) {
    let mut fired_ids: Vec<String> = Vec::new();
    let mut to_remove: Vec<String> = Vec::new();

    for task in tasks.iter() {
        let Some(&fire_at) = next_fire.get(&task.id) else {
            continue;
        };
        if now < fire_at {
            continue;
        }

        poller.enqueue(task.prompt.clone());

        if !task.recurring {
            to_remove.push(task.id.clone());
            continue;
        }

        let cron_task = task.to_cron_task();
        if is_recurring_task_aged(&cron_task, now, &config.jitter) {
            to_remove.push(task.id.clone());
            continue;
        }

        if let Some(next) = jittered_next_cron_run_ms(&task.cron, now, &task.id, &config.jitter) {
            next_fire.insert(task.id.clone(), next);
            fired_ids.push(task.id.clone());
        } else {
            to_remove.push(task.id.clone());
        }
    }

    if !fired_ids.is_empty() {
        store.mark_fired(&fired_ids, now);
    }
    if !to_remove.is_empty() {
        store.remove(&to_remove);
        tasks.retain(|t| !to_remove.contains(&t.id));
        for id in &to_remove {
            next_fire.remove(id);
        }
    }
}

fn spawn_file_watcher(
    project_root: &PathBuf,
    reload_tx: mpsc::Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{Event, EventKind, RecursiveMode, Watcher};

    let path = cron_file_path(project_root);
    let rebon_dir = cron_dir(project_root);
    if std::fs::create_dir_all(&rebon_dir).is_err() {
        return None;
    }

    let pending_generation = Arc::new(AtomicU64::new(0));
    let reload_tx_cloned = reload_tx.clone();
    let watched_path = path.clone();
    let watched_file_name = watched_path.file_name().map(|name| name.to_owned());
    let pending_generation_for_watcher = pending_generation.clone();
    let runtime = tokio::runtime::Handle::current();

    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        // Only react to changes that could affect scheduled_tasks.json. The
        // watcher is scoped to the .rebon dir (we can't watch a file that
        // may not exist yet), so filter paths here.
        let touches_cron_file = event.paths.iter().any(|p| {
            p == &watched_path
                || watched_file_name
                    .as_ref()
                    .is_some_and(|name| p.file_name() == Some(name))
        });
        if !touches_cron_file {
            return;
        }
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        // Debounce: coalesce bursts (e.g. Windows atomic rename = create + remove + modify).
        let generation = pending_generation_for_watcher.fetch_add(1, Ordering::SeqCst) + 1;
        let tx = reload_tx_cloned.clone();
        let pending = pending_generation_for_watcher.clone();
        runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(FILE_STABILITY_MS)).await;
            if pending.load(Ordering::SeqCst) == generation {
                let _ = tx.try_send(());
            }
        });
    }) {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "[CronScheduler] file watcher init failed — external writes won't hot-reload"
            );
            return None;
        }
    };

    if let Err(err) = watcher.watch(&rebon_dir, RecursiveMode::NonRecursive) {
        tracing::warn!(
            error = %err,
            dir = %rebon_dir.display(),
            "[CronScheduler] failed to start watching .rebon — external writes won't hot-reload"
        );
        return None;
    }

    Some(watcher)
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::cron::tasks::{add_cron_task, write_cron_tasks};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    fn tmp_project() -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!(
                "rebon-sched-tests-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ))
            .tempdir()
            .unwrap()
    }

    fn seed_task(dir: &TempDir, id: &str, cron: &str, recurring: bool, created_at: i64) {
        let mut tasks = read_cron_tasks(dir.path());
        tasks.push(CronTask {
            id: id.into(),
            cron: cron.into(),
            prompt: format!("prompt-{id}"),
            created_at,
            last_fired_at: None,
            recurring,
            permanent: false,
        });
        write_cron_tasks(dir.path(), &tasks).unwrap();
    }

    fn test_config(dir: &TempDir) -> SchedulerConfig {
        SchedulerConfig {
            project_root: dir.path().to_path_buf(),
            tick: Duration::from_millis(10),
            probe: Duration::from_millis(10),
            // Disable all jitter so tests are exact.
            jitter: CronJitterConfig {
                recurring_frac: 0.0,
                recurring_cap_ms: 0,
                one_shot_max_ms: 0,
                one_shot_floor_ms: 0,
                one_shot_minute_mod: 60,
                recurring_max_age_ms: 0,
            },
        }
    }

    #[test]
    fn tick_fires_one_shot_and_removes_it() {
        let dir = tmp_project();
        let id = add_cron_task(dir.path(), "0 * * * *", "run once", false, 1_000).unwrap();
        let cfg = test_config(&dir);
        let poller = CronPoller::new();
        let mut tasks = read_cron_tasks(dir.path());
        let mut next_fire = compute_next_fire_table(&tasks, 1_000, &cfg.jitter);
        // Force fire-time to "now" so the tick triggers.
        *next_fire.get_mut(&id).unwrap() = 10_000;

        run_tick(&cfg, &poller, &mut tasks, &mut next_fire, 10_000);

        assert_eq!(poller.pending_len(), 1);
        assert!(tasks.is_empty(), "one-shot removed from in-memory list");
        assert!(
            read_cron_tasks(dir.path()).is_empty(),
            "one-shot removed from disk"
        );
    }

    #[test]
    fn tick_fires_recurring_and_reschedules() {
        let dir = tmp_project();
        let id = add_cron_task(dir.path(), "0 * * * *", "hourly", true, 1_000).unwrap();
        let cfg = test_config(&dir);
        let poller = CronPoller::new();
        let mut tasks = read_cron_tasks(dir.path());
        let mut next_fire = compute_next_fire_table(&tasks, 1_000, &cfg.jitter);
        *next_fire.get_mut(&id).unwrap() = 10_000;

        run_tick(&cfg, &poller, &mut tasks, &mut next_fire, 10_000);

        assert_eq!(poller.pending_len(), 1);
        assert_eq!(tasks.len(), 1, "recurring task still present");
        let rescheduled = *next_fire.get(&id).unwrap();
        assert!(rescheduled > 10_000, "next fire is strictly after now");
        // Disk-side: last_fired_at stamped.
        let on_disk = read_cron_tasks(dir.path());
        assert_eq!(on_disk[0].last_fired_at, Some(10_000));
    }

    #[test]
    fn recurring_aged_out_deleted_after_last_fire() {
        let dir = tmp_project();
        let id = add_cron_task(dir.path(), "0 * * * *", "expiring", true, 0).unwrap();
        let cfg = SchedulerConfig {
            jitter: CronJitterConfig {
                recurring_max_age_ms: 5_000,
                ..DEFAULT_CRON_JITTER_CONFIG
            },
            ..test_config(&dir)
        };
        let poller = CronPoller::new();
        let mut tasks = read_cron_tasks(dir.path());
        let mut next_fire = HashMap::new();
        next_fire.insert(id.clone(), 10_000);

        run_tick(&cfg, &poller, &mut tasks, &mut next_fire, 10_000);

        assert_eq!(
            poller.pending_len(),
            1,
            "fires one last time before aging out"
        );
        assert!(tasks.is_empty(), "aged-out recurring removed in-memory");
        assert!(
            read_cron_tasks(dir.path()).is_empty(),
            "aged-out recurring removed from disk"
        );
    }

    #[test]
    fn surface_missed_tasks_enqueues_summary_and_prunes_one_shots() {
        let dir = tmp_project();
        // One-shot in the past.
        seed_task(&dir, "aaaabbbb", "0 9 * * *", false, 0);
        // Recurring in the past.
        seed_task(&dir, "11112222", "0 * * * *", true, 0);

        let poller = CronPoller::new();
        surface_missed_tasks(&dir.path().to_path_buf(), &poller);

        assert_eq!(poller.pending_len(), 1, "coalesced summary");
        let remaining = read_cron_tasks(dir.path());
        // One-shot removed, recurring kept untouched for the normal tick path.
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "11112222");
        assert!(remaining[0].last_fired_at.is_none());
    }

    #[test]
    fn compute_next_fire_table_picks_jitter_per_kind() {
        let dir = tmp_project();
        let recurring_id = add_cron_task(dir.path(), "0 * * * *", "hourly", true, 1_000).unwrap();
        let one_shot_id =
            add_cron_task(dir.path(), "30 14 * * *", "reminder", false, 1_000).unwrap();
        let tasks = read_cron_tasks(dir.path());
        let cfg = test_config(&dir);
        let table = compute_next_fire_table(&tasks, 1_000, &cfg.jitter);
        assert!(table.contains_key(&recurring_id));
        assert!(table.contains_key(&one_shot_id));
    }

    #[test]
    fn handle_stop_terminates_loop() {
        let dir = tmp_project();
        let cfg = test_config(&dir);
        let poller = CronPoller::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let handle = start_scheduler(cfg, poller);
            tokio::time::sleep(Duration::from_millis(50)).await;
            handle.stop().await;
        });
    }
}
