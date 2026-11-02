//! The router: the one resident session the person talks to, which sends
//! work to background worker jobs (`rebon_core::router_mode`).
//!
//! There is one per user, not one per project. It is an ordinary background
//! job that starts with `/router`, which puts its session in router mode for
//! good. It lives in its own directory under the store, because it never
//! works in a project itself. `<store>/router.json` names its job. Stopping
//! the router leaves its workers running: they are tasks in their own right.

use std::path::{Path, PathBuf};

use rebon_session_host::{BackgroundJobState, BackgroundJobStatus, BackgroundStore};
use serde::{Deserialize, Serialize};

use crate::background::{BackgroundLaunchOptions, BackgroundRuntimeFields};

const RECORD_FILE_NAME: &str = "router.json";
/// Where the supervisor publishes the router and its workers for the
/// desktop app; see `crate::router_digest`.
pub(crate) const ROSTER_FILE_NAME: &str = "router-roster.json";
const ROUTER_NAME: &str = "Rebon Router";
/// The permission mode the router and its workers run in: they act on their
/// own, with the auto-mode classifier as the check instead of a person.
pub(crate) const ROUTER_PERMISSION_MODE: &str = "auto";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouterRecord {
    job_id: String,
}

fn record_path(store: &BackgroundStore) -> PathBuf {
    store.root().join(RECORD_FILE_NAME)
}

/// Where the router's session lives. It is not a project.
pub(crate) fn router_workspace(store: &BackgroundStore) -> PathBuf {
    store.root().join("router")
}

/// The router's job, if one is recorded and has not been stopped or removed.
pub(crate) fn current_router(store: &BackgroundStore) -> Option<BackgroundJobState> {
    let record: RouterRecord = std::fs::read(record_path(store))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
    let job = store.read_state(&record.job_id).ok()?;
    (job.process.status != BackgroundJobStatus::Stopped && !job.process.removal_reserved)
        .then_some(job)
}

/// The light model the router runs: the active provider's `fast` profile,
/// else its `small` one. `None` leaves the router on the default model.
fn router_model() -> Option<String> {
    [
        rebon_types::MODEL_PROFILE_FAST,
        rebon_types::MODEL_PROFILE_SMALL,
    ]
    .into_iter()
    .find_map(rebon_config::active_provider_profile_model)
}

fn launch_options(workspace: &Path, model: Option<String>) -> BackgroundLaunchOptions {
    BackgroundLaunchOptions {
        prompt: "/router".to_string(),
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
        name: Some(ROUTER_NAME.to_string()),
        agent_type: None,
        parent_job_id: None,
    }
}

/// The router, started if there is none. `true` when this call started it.
pub(crate) fn ensure_router(store: &BackgroundStore) -> anyhow::Result<(BackgroundJobState, bool)> {
    if let Some(job) = current_router(store) {
        return Ok((job, false));
    }
    let workspace = router_workspace(store);
    std::fs::create_dir_all(&workspace)?;
    let job = crate::background::launch_background_prompt(
        store,
        launch_options(&workspace, router_model()),
    )?;
    let record = RouterRecord {
        job_id: job.identity.job_id.clone(),
    };
    rebon_session::write_file_atomically(
        &record_path(store),
        &serde_json::to_vec_pretty(&record)?,
    )?;
    Ok((job, true))
}

/// Stop the router, leaving its workers running. The stopped job's id, if
/// there was a router.
///
/// Stopping ends a turn in progress, but a router between turns is a job
/// that already finished, and a stop leaves that as it is. So the record
/// goes too, and that is what makes it no router. The roster goes with it,
/// in case no supervisor is up to remove it.
pub(crate) fn stop_router(store: &BackgroundStore) -> anyhow::Result<Option<String>> {
    let Some(mut job) = current_router(store) else {
        return Ok(None);
    };
    rebon_session_host::stop_background_job_in_store(store, &mut job)?;
    for path in [record_path(store), store.root().join(ROSTER_FILE_NAME)] {
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    Ok(Some(job.identity.job_id))
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
    format!(
        "Router {} — {:?}, model {model}, {workers} worker(s)",
        job.identity.job_id, job.process.status
    )
}

/// Record `job_id` as the router, for tests elsewhere that need one.
#[cfg(test)]
pub(crate) fn record_router_for_test(store: &BackgroundStore, job_id: &str) {
    let record = RouterRecord {
        job_id: job_id.to_string(),
    };
    std::fs::write(record_path(store), serde_json::to_vec(&record).unwrap()).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_router_runs_in_its_own_directory_in_auto_mode_and_starts_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let options = launch_options(&router_workspace(&store), Some("light-model".into()));
        assert_eq!(options.prompt, "/router");
        assert_eq!(options.cwd, dir.path().join("router"));
        assert_eq!(options.runtime.permission_mode.as_deref(), Some("auto"));
        assert_eq!(options.runtime.model.as_deref(), Some("light-model"));
        assert!(!options.isolate_in_worktree);
        assert_eq!(options.parent_job_id, None);
    }

    #[test]
    fn a_stopped_or_missing_router_is_no_router() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        assert!(current_router(&store).is_none());

        let mut job = store
            .create_job(
                "/router".into(),
                router_workspace(&store),
                launch_options(&router_workspace(&store), None).runtime,
            )
            .unwrap();
        let record = RouterRecord {
            job_id: job.identity.job_id.clone(),
        };
        std::fs::write(record_path(&store), serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(
            current_router(&store).map(|router| router.identity.job_id),
            Some(job.identity.job_id.clone())
        );

        job.process.status = BackgroundJobStatus::Stopped;
        store.write_state(&job).unwrap();
        assert!(current_router(&store).is_none());
    }

    #[test]
    fn stopping_a_router_between_turns_leaves_no_router() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "/router".into(),
                router_workspace(&store),
                launch_options(&router_workspace(&store), None).runtime,
            )
            .unwrap();
        job.process.status = BackgroundJobStatus::Succeeded;
        store.write_state(&job).unwrap();
        record_router_for_test(&store, &job.identity.job_id);
        std::fs::write(store.root().join(ROSTER_FILE_NAME), "{}").unwrap();

        assert_eq!(stop_router(&store).unwrap(), Some(job.identity.job_id));
        assert!(current_router(&store).is_none());
        assert!(!store.root().join(ROSTER_FILE_NAME).exists());
        assert_eq!(stop_router(&store).unwrap(), None);
    }
}
