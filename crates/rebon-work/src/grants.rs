//! What the person authorized the router's workers to do, as structured
//! grants recorded on disk. An issuing router's restart invalidates them.
//!
//! A grant names one risky step — an action and the specific thing that makes
//! it risky (`git push` to `main`) — and the router tasks it covers. It is
//! issued by the router's host, and only after a verifier found the step in
//! the person's own words (see `rebon_core::auto_mode_classifier::grant_verifier`).
//! A worker's auto-mode gate then reads the grants for its task, so a step the
//! person already named is not asked about again.
//!
//! `<home>/work/grants.json` is the record. Several processes write it (the
//! router's host issuing, a `rebon router revoke`), so every change goes
//! through [`update_grants`], which holds `grants.lock` while it reads,
//! changes and writes. File tools of background jobs are refused the whole
//! `work/` directory; a shell command is still left to the classifier until
//! workers run sandboxed.
//!
//! A grant lives as long as its tasks: once every task it names has settled,
//! it no longer applies. [`MAX_GRANT_AGE_MS`] caps it regardless, for a task
//! that never settles.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::{work_dir, Ledger, Task};

pub const GRANTS_FILE_NAME: &str = "grants.json";
const GRANTS_LOCK_FILE_NAME: &str = "grants.lock";

/// The longest a grant applies, whatever its tasks do.
pub const MAX_GRANT_AGE_MS: u64 = 24 * 60 * 60 * 1000;
/// How long a dead grant stays in the file, for `rebon router grants`.
const KEEP_DEAD_GRANTS_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const LOCK_WAIT: Duration = Duration::from_secs(3);

pub fn grants_path(home: &Path) -> PathBuf {
    work_dir(home).join(GRANTS_FILE_NAME)
}

/// One authorized step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub id: String,
    /// The router whose workers it covers.
    pub router_job_id: String,
    /// The router process that saw the user's words. A restarted router has
    /// no in-memory source and must not inherit an older process's grants.
    pub router_pid: u32,
    pub router_pid_identity: String,
    /// The project directory of the tasks it covers.
    pub project: String,
    /// The tasks it covers. It lives while any of them is unsettled.
    pub task_ids: Vec<String>,
    /// What may be done, such as `git push`.
    pub action: String,
    /// The specific thing that makes it risky, such as `branch main`.
    pub target: String,
    /// One line for the person: what was authorized.
    pub summary: String,
    /// The person's own words it rests on, verbatim.
    pub user_quote: String,
    /// When the person's words were a "yes": the router's proposal they
    /// answered, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_quote: Option<String>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
    /// A later live user message invalidated this grant.
    #[serde(default)]
    pub consent_withdrawn: bool,
}

/// Why a grant no longer applies, or that it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantState {
    Live,
    Revoked,
    Expired,
    /// Every task it names has settled.
    TasksSettled,
}

impl GrantState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::TasksSettled => "tasks settled",
        }
    }
}

impl Grant {
    /// Where it stands now. A task the ledger does not know yet (planned a
    /// moment ago, not yet applied by the supervisor) counts as unsettled.
    pub fn state(&self, ledger: &Ledger, now_ms: u64) -> GrantState {
        if self.revoked_at_ms.is_some() {
            return GrantState::Revoked;
        }
        if now_ms >= self.expires_at_ms {
            return GrantState::Expired;
        }
        let all_settled = !self.task_ids.is_empty()
            && self.task_ids.iter().all(|id| {
                ledger
                    .tasks
                    .get(id)
                    .is_some_and(|task| task.status.is_settled())
            });
        if all_settled {
            GrantState::TasksSettled
        } else {
            GrantState::Live
        }
    }

    /// Whether it covers `task`: live, the same router, one of its tasks,
    /// the same project.
    pub fn covers(&self, task: &Task, ledger: &Ledger, now_ms: u64) -> bool {
        self.state(ledger, now_ms) == GrantState::Live
            && task.router_job_id.as_deref() == Some(self.router_job_id.as_str())
            && self.task_ids.iter().any(|id| id == &task.id)
            && same_project(&task.project, &self.project)
    }

    /// When it stopped applying, for pruning: revocation, expiry, or the
    /// last of its tasks settling.
    fn ended_at_ms(&self, ledger: &Ledger, now_ms: u64) -> Option<u64> {
        match self.state(ledger, now_ms) {
            GrantState::Live => None,
            GrantState::Revoked => self.revoked_at_ms,
            GrantState::Expired => Some(self.expires_at_ms),
            GrantState::TasksSettled => self
                .task_ids
                .iter()
                .filter_map(|id| ledger.tasks.get(id).map(|task| task.updated_at_ms))
                .max(),
        }
    }
}

fn same_project(a: &str, b: &str) -> bool {
    let trim = |path: &str| path.trim().trim_end_matches(['/', '\\']).to_string();
    let (a, b) = (trim(a), trim(b));
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantBook {
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub router_user_epoch: std::collections::BTreeMap<String, u64>,
}

impl GrantBook {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a grant record", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        rebon_session::write_file_atomically(path, &serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {}", path.display()))
    }

    /// The grants that cover the task `worker_job_id` is the worker of.
    /// An acceptance check is not the task's worker and gets none.
    pub fn for_worker<'a>(
        &'a self,
        ledger: &Ledger,
        worker_job_id: &str,
        now_ms: u64,
    ) -> Vec<&'a Grant> {
        let Some(task) = ledger.tasks.values().find(|task| {
            task.worker_job_id.as_deref() == Some(worker_job_id) && task.handed_on_to.is_none()
        }) else {
            return Vec::new();
        };
        self.grants
            .iter()
            .filter(|grant| grant.covers(task, ledger, now_ms))
            .collect()
    }

    /// A new live user message fences all earlier grants for this router.
    pub fn revoke_on_user_message(&mut self, router: &str, ledger: &Ledger, now_ms: u64) {
        *self
            .router_user_epoch
            .entry(router.to_string())
            .or_default() += 1;
        for grant in &mut self.grants {
            if grant.router_job_id == router && grant.state(ledger, now_ms) == GrantState::Live {
                grant.revoked_at_ms = Some(now_ms);
                grant.consent_withdrawn = true;
            }
        }
    }

    pub fn user_epoch(&self, router: &str) -> u64 {
        self.router_user_epoch.get(router).copied().unwrap_or(0)
    }

    pub fn denied_by_later_message(&self, router: &str, action: &str, target: &str) -> bool {
        self.grants.iter().any(|grant| {
            grant.router_job_id == router
                && grant.consent_withdrawn
                && grant.action == action
                && grant.target == target
        })
    }

    pub fn router_revoked_for_worker(&self, ledger: &Ledger, worker_job_id: &str) -> bool {
        ledger.tasks.values().any(|task| {
            task.worker_job_id.as_deref() == Some(worker_job_id)
                && task.handed_on_to.is_none()
                && task.router_job_id.as_deref().is_some_and(|router| {
                    self.grants
                        .iter()
                        .any(|grant| grant.router_job_id == router && grant.consent_withdrawn)
                })
        })
    }

    /// Revoke the router's live grants with these ids, or all of them when
    /// `ids` is empty. Returns the ids revoked.
    pub fn revoke(
        &mut self,
        router_job_id: Option<&str>,
        ids: &[String],
        ledger: &Ledger,
        now_ms: u64,
    ) -> Vec<String> {
        let mut revoked = Vec::new();
        for grant in &mut self.grants {
            let ours = router_job_id.is_none_or(|router| grant.router_job_id == router);
            let named = ids.is_empty() || ids.iter().any(|id| id == &grant.id);
            if ours && named && grant.state(ledger, now_ms) == GrantState::Live {
                grant.revoked_at_ms = Some(now_ms);
                grant.consent_withdrawn = true;
                revoked.push(grant.id.clone());
            }
        }
        revoked
    }

    fn prune(&mut self, ledger: &Ledger, now_ms: u64) {
        self.grants.retain(|grant| {
            (grant.consent_withdrawn
                && grant.task_ids.iter().any(|id| {
                    ledger
                        .tasks
                        .get(id)
                        .is_none_or(|task| !task.status.is_settled())
                }))
                || grant
                    .ended_at_ms(ledger, now_ms)
                    .is_none_or(|ended| now_ms.saturating_sub(ended) < KEEP_DEAD_GRANTS_MS)
        });
    }
}

/// Read `<home>/work/grants.json`, change it, and write it back, holding the
/// lock throughout. Dead grants past their keep time are dropped on the way.
pub fn update_grants<R>(
    home: &Path,
    ledger: &Ledger,
    now_ms: u64,
    change: impl FnOnce(&mut GrantBook) -> R,
) -> anyhow::Result<R> {
    let path = grants_path(home);
    let _lock = acquire_grants_lock(&work_dir(home).join(GRANTS_LOCK_FILE_NAME))?;
    let mut book = GrantBook::load(&path)?;
    let result = change(&mut book);
    book.prune(ledger, now_ms);
    book.save(&path)?;
    Ok(result)
}

pub fn load_grants(home: &Path) -> GrantBook {
    GrantBook::load(&grants_path(home)).unwrap_or_default()
}

pub fn new_grant_id(now_ms: u64) -> String {
    format!("g-{}", crate::new_task_id(now_ms).trim_start_matches("t-"))
}

/// An OS file lock, released when its owning process or file handle exits.
/// Never delete a lock merely because its mtime is old: a paused writer
/// could otherwise overwrite a newer revocation when it resumes.
fn acquire_grants_lock(path: &Path) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("lock {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewTask, TaskStatus};

    const NOW: u64 = 1_000_000_000;

    fn grant(id: &str, router: &str, project: &str, tasks: &[&str]) -> Grant {
        Grant {
            id: id.into(),
            router_job_id: router.into(),
            router_pid: std::process::id(),
            router_pid_identity: "test-process".into(),
            project: project.into(),
            task_ids: tasks.iter().map(|task| task.to_string()).collect(),
            action: "git push".into(),
            target: "branch main".into(),
            summary: "push the fix to main".into(),
            user_quote: "push it to main".into(),
            proposal_quote: None,
            issued_at_ms: NOW,
            expires_at_ms: NOW + MAX_GRANT_AGE_MS,
            revoked_at_ms: None,
            consent_withdrawn: false,
        }
    }

    fn ledger_with(tasks: &[(&str, &str, &str, Option<&str>, TaskStatus)]) -> Ledger {
        let mut ledger = Ledger::default();
        for (id, router, project, worker, status) in tasks {
            let mut task = NewTask {
                id: id.to_string(),
                title: id.to_string(),
                project: project.to_string(),
                goal: "goal".into(),
                done_when: String::new(),
                depends_on: Vec::new(),
                isolate: false,
                model_profile: None,
                worker: None,
            }
            .into_task(Some(router.to_string()), NOW);
            task.worker_job_id = worker.map(str::to_string);
            task.status = *status;
            ledger.tasks.insert(id.to_string(), task);
        }
        ledger
    }

    #[test]
    fn a_grant_covers_only_its_own_tasks_router_and_project() {
        let ledger = ledger_with(&[
            (
                "t-1",
                "router-a",
                "/work/app",
                Some("w-1"),
                TaskStatus::Running,
            ),
            (
                "t-2",
                "router-a",
                "/work/app",
                Some("w-2"),
                TaskStatus::Running,
            ),
            (
                "t-3",
                "router-b",
                "/work/app",
                Some("w-3"),
                TaskStatus::Running,
            ),
            (
                "t-4",
                "router-a",
                "/work/other",
                Some("w-4"),
                TaskStatus::Running,
            ),
        ]);
        let book = GrantBook {
            grants: vec![grant(
                "g-1",
                "router-a",
                "/work/app/",
                &["t-1", "t-3", "t-4"],
            )],
            ..GrantBook::default()
        };
        assert_eq!(book.for_worker(&ledger, "w-1", NOW).len(), 1);
        assert!(
            book.for_worker(&ledger, "w-2", NOW).is_empty(),
            "not its task"
        );
        assert!(
            book.for_worker(&ledger, "w-3", NOW).is_empty(),
            "another router's"
        );
        assert!(
            book.for_worker(&ledger, "w-4", NOW).is_empty(),
            "another project"
        );
        assert!(book.for_worker(&ledger, "nobody", NOW).is_empty());
    }

    #[test]
    fn later_user_message_retracts_grants_across_workers_and_cannot_be_reissued() {
        let ledger = ledger_with(&[
            (
                "t-1",
                "router-a",
                "/work/app",
                Some("w-1"),
                TaskStatus::Running,
            ),
            (
                "t-2",
                "router-a",
                "/work/app",
                Some("w-2"),
                TaskStatus::Running,
            ),
            (
                "t-3",
                "router-b",
                "/work/app",
                Some("w-3"),
                TaskStatus::Running,
            ),
        ]);
        let mut book = GrantBook {
            grants: vec![
                grant("g-a", "router-a", "/work/app", &["t-1", "t-2"]),
                grant("g-b", "router-b", "/work/app", &["t-3"]),
            ],
            ..GrantBook::default()
        };
        let prior_epoch = book.user_epoch("router-a");
        book.revoke_on_user_message("router-a", &ledger, NOW);
        assert_ne!(book.user_epoch("router-a"), prior_epoch);
        assert!(book.for_worker(&ledger, "w-1", NOW).is_empty());
        assert!(book.for_worker(&ledger, "w-2", NOW).is_empty());
        assert_eq!(book.for_worker(&ledger, "w-3", NOW).len(), 1);
        assert!(book.router_revoked_for_worker(&ledger, "w-1"));
        assert!(book.router_revoked_for_worker(&ledger, "w-2"));
        assert!(!book.router_revoked_for_worker(&ledger, "w-3"));
        assert!(book.denied_by_later_message("router-a", "git push", "branch main"));
        assert!(!book.denied_by_later_message("router-b", "git push", "branch main"));
        book.prune(&ledger, NOW + KEEP_DEAD_GRANTS_MS + 1);
        assert!(book.denied_by_later_message("router-a", "git push", "branch main"));
    }

    #[test]
    fn a_grant_ends_with_its_tasks_its_age_or_a_revocation() {
        let mut ledger = ledger_with(&[
            (
                "t-1",
                "router-a",
                "/work/app",
                Some("w-1"),
                TaskStatus::Running,
            ),
            (
                "t-2",
                "router-a",
                "/work/app",
                Some("w-2"),
                TaskStatus::Done,
            ),
        ]);
        let live = grant("g-1", "router-a", "/work/app", &["t-1", "t-2"]);
        assert_eq!(live.state(&ledger, NOW), GrantState::Live);
        assert_eq!(
            live.state(&ledger, NOW + MAX_GRANT_AGE_MS),
            GrantState::Expired
        );

        ledger.tasks.get_mut("t-1").unwrap().status = TaskStatus::Cancelled;
        assert_eq!(live.state(&ledger, NOW), GrantState::TasksSettled);

        let pending = grant("g-2", "router-a", "/work/app", &["t-not-applied-yet"]);
        assert_eq!(pending.state(&ledger, NOW), GrantState::Live);

        let mut book = GrantBook {
            grants: vec![pending],
            ..GrantBook::default()
        };
        assert!(book.revoke(Some("router-b"), &[], &ledger, NOW).is_empty());
        assert_eq!(book.revoke(Some("router-a"), &[], &ledger, NOW), ["g-2"]);
        assert_eq!(book.grants[0].state(&ledger, NOW), GrantState::Revoked);
        assert!(book.grants[0].consent_withdrawn);
        assert!(book.denied_by_later_message("router-a", "git push", "branch main"));
        assert!(
            book.revoke(None, &["g-2".to_string()], &ledger, NOW)
                .is_empty(),
            "already revoked"
        );
    }

    #[test]
    fn an_acceptance_check_gets_no_grant() {
        let mut ledger = ledger_with(&[(
            "t-1",
            "router-a",
            "/work/app",
            Some("w-1"),
            TaskStatus::Running,
        )]);
        ledger.tasks.get_mut("t-1").unwrap().verifier_job_id = Some("v-1".into());
        let book = GrantBook {
            grants: vec![grant("g-1", "router-a", "/work/app", &["t-1"])],
            ..GrantBook::default()
        };
        assert!(book.for_worker(&ledger, "v-1", NOW).is_empty());
    }

    #[test]
    fn updates_round_trip_through_the_file_and_drop_long_dead_grants() {
        let home = tempfile::tempdir().unwrap();
        let ledger = Ledger::default();
        update_grants(home.path(), &ledger, NOW, |book| {
            book.grants
                .push(grant("g-1", "router-a", "/work/app", &["t-1"]));
            let mut old = grant("g-old", "router-a", "/work/app", &["t-1"]);
            old.revoked_at_ms = Some(NOW - KEEP_DEAD_GRANTS_MS - 1);
            book.grants.push(old);
        })
        .unwrap();
        let book = load_grants(home.path());
        assert_eq!(
            book.grants
                .iter()
                .map(|grant| grant.id.as_str())
                .collect::<Vec<_>>(),
            ["g-1"]
        );
        assert!(work_dir(home.path()).join(GRANTS_LOCK_FILE_NAME).exists());
    }

    #[test]
    fn a_held_lock_makes_an_update_fail_rather_than_race() {
        let home = tempfile::tempdir().unwrap();
        let lock = work_dir(home.path()).join(GRANTS_LOCK_FILE_NAME);
        let held = acquire_grants_lock(&lock).unwrap();
        let refused = update_grants(home.path(), &Ledger::default(), NOW, |_| ());
        assert!(refused.is_err());
        drop(held);
        update_grants(home.path(), &Ledger::default(), NOW, |_| ()).unwrap();
    }
}
