//! The router's workers in their project's group ([`rebon_group::router`]),
//! as the session host keeps them.
//!
//! A worker joins in its own process, once its session exists and before
//! its first turn reads the group inbox ([`join`]); the supervisor takes the
//! ones whose job is over out again ([`leave_finished`]). A check the router
//! runs in a worker's worktree is in the project's group too: a worktree is
//! a copy of the project, not a project of its own.

use std::path::{Path, PathBuf};

use rebon_group::GroupStore;
use rebon_session_host::{BackgroundJobState, BackgroundJobStatus, BackgroundStore};

use crate::worker_control::{recorded_job, ROUTER_RECORD_FILE};

/// The groups plugin's id: `plugins.groups.enabled: false` takes a session
/// out of groups, a worker's too.
const GROUPS_PLUGIN_ID: &str = "groups";
/// How long a worker stays after its last turn ended: the hour it lingers
/// warm, when it is a good one for follow-up work. Sent more work later, it
/// joins again.
pub const STAYS_AFTER_LAST_TURN_MS: u64 = 60 * 60 * 1000;

fn groups(store: &BackgroundStore) -> GroupStore {
    GroupStore::new(rebon_group::default_root(store.root()))
}

/// Puts `job`, when it is one of the router's workers, in its project's
/// router group as `session_id`. Anything else — a job that is not the
/// router's, groups switched off for the project, a session the person put
/// in another group — is left as it is.
pub fn join(store: &BackgroundStore, job: &BackgroundJobState, session_id: &str) {
    let Some(parent) = job.identity.parent_job_id.as_deref() else {
        return;
    };
    if recorded_job(store.root(), ROUTER_RECORD_FILE).as_deref() != Some(parent) {
        return;
    }
    let project = project_of(Path::new(&job.identity.cwd));
    let switches = rebon_config::saved_plugin_switches_in(store.root(), &project);
    if switches.get(GROUPS_PLUGIN_ID) == Some(&false) {
        return;
    }
    let project = project.to_string_lossy();
    match rebon_group::router::join_worker(&groups(store), &project, session_id, &job.identity.name)
    {
        Ok(Some(group)) => tracing::debug!(
            job_id = %job.identity.job_id,
            group = %group.id,
            "router worker is in its project's group"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(
            job_id = %job.identity.job_id,
            error = %format!("{error:#}"),
            "router worker could not join its project's group"
        ),
    }
}

/// Takes out of the router groups every worker no job in `jobs` is still
/// working as: running, waiting, or finished less than
/// [`STAYS_AFTER_LAST_TURN_MS`] ago.
pub fn leave_finished(store: &BackgroundStore, jobs: &[BackgroundJobState], now_ms: u64) {
    let working = |session_id: &str| {
        jobs.iter().any(|job| {
            job.identity.session_id.as_deref() == Some(session_id) && still_working(job, now_ms)
        })
    };
    match rebon_group::router::leave_finished(&groups(store), &working) {
        Ok(0) => {}
        Ok(left) => tracing::debug!(left, "router workers left their project's group"),
        Err(error) => tracing::warn!(
            error = %format!("{error:#}"),
            "could not take finished router workers out of their group"
        ),
    }
}

fn still_working(job: &BackgroundJobState, now_ms: u64) -> bool {
    match job.process.status {
        BackgroundJobStatus::Queued
        | BackgroundJobStatus::Running
        | BackgroundJobStatus::NeedsInput
        | BackgroundJobStatus::Idle => true,
        BackgroundJobStatus::Succeeded => {
            now_ms.saturating_sub(job.process.updated_at_ms) < STAYS_AFTER_LAST_TURN_MS
        }
        BackgroundJobStatus::Failed | BackgroundJobStatus::Stopped => false,
    }
}

/// The project `cwd` is in: itself, or for a worktree Rebon made
/// (`<project>/.rebon/worktrees/<name>`), the project it was made from.
fn project_of(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find_map(|dir| {
            let worktrees = dir.parent()?;
            let rebon = worktrees.parent()?;
            (worktrees.file_name()? == "worktrees" && rebon.file_name()? == ".rebon")
                .then(|| rebon.parent().map(Path::to_path_buf))
                .flatten()
        })
        .unwrap_or_else(|| cwd.to_path_buf())
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

    fn job(store: &BackgroundStore, cwd: &Path, parent: Option<&str>) -> BackgroundJobState {
        let mut job = store
            .create_job_with_name(
                "brief".into(),
                cwd.to_path_buf(),
                runtime(),
                Some("Fix the build".into()),
            )
            .unwrap();
        job.identity.parent_job_id = parent.map(str::to_string);
        job
    }

    fn members(store: &BackgroundStore) -> Vec<(String, String)> {
        groups(store)
            .list()
            .unwrap()
            .into_iter()
            .flat_map(|group| {
                group
                    .members
                    .into_iter()
                    .map(move |member| (group.cwd.clone(), member.session_id))
            })
            .collect()
    }

    #[test]
    fn the_routers_workers_join_their_projects_group_and_no_one_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path().join("home"));
        let project = dir.path().join("app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(
            store.root().join(ROUTER_RECORD_FILE),
            r#"{"jobId":"router-job"}"#,
        )
        .unwrap();

        join(&store, &job(&store, &project, None), "lone");
        join(&store, &job(&store, &project, Some("other-job")), "other");
        assert!(members(&store).is_empty());

        join(&store, &job(&store, &project, Some("router-job")), "w1");
        let worktree = project.join(".rebon").join("worktrees").join("bg-1");
        join(&store, &job(&store, &worktree, Some("router-job")), "check");
        let project = project.to_string_lossy().to_string();
        assert_eq!(
            members(&store),
            [(project.clone(), "w1".into()), (project, "check".into())]
        );
    }

    #[test]
    fn a_worker_stays_while_it_may_get_more_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path().join("home"));
        let mut worker = job(&store, dir.path(), None);
        let now = 10 * STAYS_AFTER_LAST_TURN_MS;
        for (status, working) in [
            (BackgroundJobStatus::Running, true),
            (BackgroundJobStatus::Idle, true),
            (BackgroundJobStatus::Failed, false),
            (BackgroundJobStatus::Stopped, false),
        ] {
            worker.process.status = status;
            assert_eq!(still_working(&worker, now), working, "{status:?}");
        }
        worker.process.status = BackgroundJobStatus::Succeeded;
        worker.process.updated_at_ms = now - 1_000;
        assert!(still_working(&worker, now));
        worker.process.updated_at_ms = now - STAYS_AFTER_LAST_TURN_MS;
        assert!(!still_working(&worker, now));
    }

    #[test]
    fn a_worktree_belongs_to_the_project_it_was_made_from() {
        let project = Path::new("/w/app");
        assert_eq!(
            project_of(&project.join(".rebon").join("worktrees").join("bg-1")),
            project
        );
        assert_eq!(
            project_of(
                &project
                    .join(".rebon")
                    .join("worktrees")
                    .join("bg-1")
                    .join("src")
            ),
            project
        );
        assert_eq!(project_of(project), project);
        assert_eq!(
            project_of(&project.join("worktrees").join("x")),
            project.join("worktrees").join("x")
        );
    }
}
