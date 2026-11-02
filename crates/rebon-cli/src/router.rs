//! The router and its watchdog: the two resident sessions of the
//! super-agent (`rebon_core::router_mode`, `rebon_core::watchdog_mode`).
//!
//! There is one router per user, not one per project: the session the person
//! talks to, which sends work to background worker jobs. Beside it runs one
//! watchdog, outside its loop, which the supervisor wakes when something in
//! that loop looks wrong (`crate::watchdog`). Each is an ordinary background
//! job whose first prompt (`/router`, `/watchdog`) puts its session in its
//! mode for good. Each lives in its own directory under the store, because
//! neither works in a project, and a record file names its job. They start and
//! stop together; stopping them leaves the router's workers running, because
//! those are tasks in their own right.

use std::path::{Path, PathBuf};

use rebon_session_host::{BackgroundJobState, BackgroundJobStatus, BackgroundStore};
use serde::{Deserialize, Serialize};

use crate::background::{BackgroundLaunchOptions, BackgroundRuntimeFields};

/// Where the supervisor publishes the router and its workers for the
/// desktop app; see `crate::router_digest`.
pub(crate) const ROSTER_FILE_NAME: &str = "router-roster.json";
/// The permission mode the router, the watchdog and the workers run in: they
/// act on their own, with the auto-mode classifier as the check instead of a
/// person.
pub(crate) const ROUTER_PERMISSION_MODE: &str = "auto";

/// One of the two resident sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resident {
    record_file: &'static str,
    name: &'static str,
    command: &'static str,
    workspace: &'static str,
}

pub(crate) const ROUTER: Resident = Resident {
    record_file: crate::session::worker_control::ROUTER_RECORD_FILE,
    name: "Rebon Router",
    command: "/router",
    workspace: "router",
};

pub(crate) const WATCHDOG: Resident = Resident {
    record_file: crate::session::worker_control::WATCHDOG_RECORD_FILE,
    name: "Rebon Watchdog",
    command: "/watchdog",
    workspace: "watchdog",
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    job_id: String,
}

fn record_path(store: &BackgroundStore, resident: Resident) -> PathBuf {
    store.root().join(resident.record_file)
}

/// Where a resident session lives. It is not a project.
pub(crate) fn workspace(store: &BackgroundStore, resident: Resident) -> PathBuf {
    store.root().join(resident.workspace)
}

/// Where the router's session lives.
pub(crate) fn router_workspace(store: &BackgroundStore) -> PathBuf {
    workspace(store, ROUTER)
}

/// The resident's job, if one is recorded and has not been stopped or
/// removed.
pub(crate) fn current(store: &BackgroundStore, resident: Resident) -> Option<BackgroundJobState> {
    let record: Record = std::fs::read(record_path(store, resident))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
    let job = store.read_state(&record.job_id).ok()?;
    (job.process.status != BackgroundJobStatus::Stopped && !job.process.removal_reserved)
        .then_some(job)
}

/// The router's job, if one runs.
pub(crate) fn current_router(store: &BackgroundStore) -> Option<BackgroundJobState> {
    current(store, ROUTER)
}

/// The light model both run: the active provider's `fast` profile, else its
/// `small` one. `None` leaves them on the default model.
fn light_model() -> Option<String> {
    [
        rebon_types::MODEL_PROFILE_FAST,
        rebon_types::MODEL_PROFILE_SMALL,
    ]
    .into_iter()
    .find_map(rebon_config::active_provider_profile_model)
}

fn launch_options(
    resident: Resident,
    workspace: &Path,
    model: Option<String>,
) -> BackgroundLaunchOptions {
    BackgroundLaunchOptions {
        prompt: resident.command.to_string(),
        images: Vec::new(),
        cwd: workspace.to_path_buf(),
        isolate_in_worktree: false,
        require_worktree: false,
        preserve_worktree_on_success: false,
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
            permission_mode: Some(ROUTER_PERMISSION_MODE.to_string()),
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        },
        name: Some(resident.name.to_string()),
        agent_type: None,
        parent_job_id: None,
    }
}

/// The resident, started if there is none. `true` when this call started it.
fn ensure(
    store: &BackgroundStore,
    resident: Resident,
) -> anyhow::Result<(BackgroundJobState, bool)> {
    if let Some(job) = current(store, resident) {
        return Ok((job, false));
    }
    let workspace = workspace(store, resident);
    std::fs::create_dir_all(&workspace)?;
    let job = crate::background::launch_background_prompt(
        store,
        launch_options(resident, &workspace, light_model()),
    )?;
    let record = Record {
        job_id: job.identity.job_id.clone(),
    };
    rebon_session::write_file_atomically(
        &record_path(store, resident),
        &serde_json::to_vec_pretty(&record)?,
    )?;
    Ok((job, true))
}

/// The router and its watchdog, each started if there is none. The router,
/// and `true` when this call started it.
pub(crate) fn ensure_router(store: &BackgroundStore) -> anyhow::Result<(BackgroundJobState, bool)> {
    let router = ensure(store, ROUTER)?;
    ensure(store, WATCHDOG)?;
    Ok(router)
}

/// Stop one resident. Stopping ends a turn in progress, but a session
/// between turns is a job that already finished, and a stop leaves that as
/// it is. So the record goes too, and that is what makes it gone.
fn stop(store: &BackgroundStore, resident: Resident) -> anyhow::Result<Option<String>> {
    let Some(mut job) = current(store, resident) else {
        return Ok(None);
    };
    rebon_session_host::stop_background_job_in_store(store, &mut job)?;
    remove_if_there(&record_path(store, resident))?;
    Ok(Some(job.identity.job_id))
}

fn remove_if_there(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Stop the router and its watchdog, leaving the router's workers running.
/// The router's job id, if there was a router. The roster goes too, in case
/// no supervisor is up to remove it.
pub(crate) fn stop_router(store: &BackgroundStore) -> anyhow::Result<Option<String>> {
    stop(store, WATCHDOG)?;
    let router = stop(store, ROUTER)?;
    remove_if_there(&store.root().join(ROSTER_FILE_NAME))?;
    Ok(router)
}

/// What `rebon router status` prints.
pub(crate) fn describe_router(store: &BackgroundStore) -> String {
    let Some(job) = current_router(store) else {
        return "No router is running. Start one with `rebon router start`.".to_string();
    };
    let model = job
        .identity
        .runtime
        .model
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let workers = store
        .list_jobs()
        .map(|jobs| {
            jobs.iter()
                .filter(|worker| {
                    worker.identity.parent_job_id.as_deref() == Some(job.identity.job_id.as_str())
                })
                .count()
        })
        .unwrap_or(0);
    let watchdog = match current(store, WATCHDOG) {
        Some(watchdog) => format!(
            "watchdog {} — {:?}",
            watchdog.identity.job_id, watchdog.process.status
        ),
        None => "no watchdog".to_string(),
    };
    format!(
        "Router {} — {:?}, model {model}, {workers} worker(s); {watchdog}",
        job.identity.job_id, job.process.status
    )
}

/// Record `job_id` as the router, for tests elsewhere that need one.
#[cfg(test)]
pub(crate) fn record_router_for_test(store: &BackgroundStore, job_id: &str) {
    record_for_test(store, ROUTER, job_id);
}

#[cfg(test)]
pub(crate) fn record_for_test(store: &BackgroundStore, resident: Resident, job_id: &str) {
    let record = Record {
        job_id: job_id.to_string(),
    };
    std::fs::write(
        record_path(store, resident),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_resident_runs_in_its_own_directory_in_auto_mode_and_starts_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let options = launch_options(
            ROUTER,
            &router_workspace(&store),
            Some("light-model".into()),
        );
        assert_eq!(options.prompt, "/router");
        assert_eq!(options.cwd, dir.path().join("router"));
        assert_eq!(options.runtime.permission_mode.as_deref(), Some("auto"));
        assert_eq!(options.runtime.model.as_deref(), Some("light-model"));
        assert!(!options.isolate_in_worktree);
        assert_eq!(options.parent_job_id, None);
        let watchdog = launch_options(WATCHDOG, &workspace(&store, WATCHDOG), None);
        assert_eq!(watchdog.prompt, "/watchdog");
        assert_eq!(watchdog.cwd, dir.path().join("watchdog"));
        assert_eq!(watchdog.name.as_deref(), Some("Rebon Watchdog"));
    }

    fn resident_job(store: &BackgroundStore, resident: Resident) -> BackgroundJobState {
        let mut job = store
            .create_job(
                resident.command.into(),
                workspace(store, resident),
                launch_options(resident, &workspace(store, resident), None).runtime,
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        store.write_state(&job).unwrap();
        record_for_test(store, resident, &job.identity.job_id);
        job
    }

    #[test]
    fn a_stopped_or_missing_router_is_no_router() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        assert!(current_router(&store).is_none());
        let mut job = resident_job(&store, ROUTER);
        assert_eq!(
            current_router(&store).map(|router| router.identity.job_id),
            Some(job.identity.job_id.clone())
        );
        job.process.status = BackgroundJobStatus::Stopped;
        store.write_state(&job).unwrap();
        assert!(current_router(&store).is_none());
    }

    #[test]
    fn stopping_the_router_between_turns_takes_its_watchdog_and_roster_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let router = resident_job(&store, ROUTER);
        resident_job(&store, WATCHDOG);
        std::fs::write(store.root().join(ROSTER_FILE_NAME), "{}").unwrap();

        assert_eq!(stop_router(&store).unwrap(), Some(router.identity.job_id));
        assert!(current_router(&store).is_none());
        assert!(current(&store, WATCHDOG).is_none());
        assert!(!store.root().join(ROSTER_FILE_NAME).exists());
        assert_eq!(stop_router(&store).unwrap(), None);
    }
}
