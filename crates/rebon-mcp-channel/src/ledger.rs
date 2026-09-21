//! `mcp-channel.json`: which jobs this surface started, who pushes for them,
//! and what has already been pushed.
//!
//! A file in the job's own directory, so it is deleted with the job
//! (`rebon rm`) and needs no cleaner of its own. It is written only by
//! `rebon mcp serve` processes and read only by them; nothing in the job
//! record (`state.json`) changes, because that record's blocks each have one
//! writer and this surface is not one of them.
//!
//! It answers three questions:
//!
//! 1. **Is this job ours to show?** Tools act only on jobs that carry a
//!    ledger — a client of this server cannot page through the user's other
//!    Rebon sessions by guessing ids.
//! 2. **Who pushes?** The server that started the job owns it. Another server
//!    takes it over only if the owner process is gone — which is what a client
//!    restarting its MCP server looks like.
//! 3. **Was this pushed already?** Every push is claimed here under a lock
//!    before it is written, so two servers, or one server restarted, never
//!    push the same thing twice.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::Context;
use fs2::FileExt;
use rebon_session_host::BackgroundStore;
use serde::{Deserialize, Serialize};

const LEDGER_FILE: &str = "mcp-channel.json";
const LEDGER_LOCK: &str = "mcp-channel.lock";
const LEDGER_VERSION: u32 = 1;
/// Deliveries kept per job. A job pushes a handful of times per turn; the
/// cap is for a job someone has replied to for weeks.
const MAX_DELIVERIES: usize = 256;

/// The process that pushes for a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerOwner {
    pub pid: u32,
    /// Which process instance that pid was (start time, platform-specific).
    /// A pid alone would let a reused pid keep an orphaned job forever.
    pub pid_identity: Option<String>,
}

impl LedgerOwner {
    pub fn this_process() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            pid_identity: rebon_session_host::process_identity(pid),
        }
    }
}

/// Whether `owner` is still the process it was. Anything short of a clear
/// "gone" counts as alive: taking a job away from a live server would move
/// its pushes into a session that did not ask for them.
pub(crate) fn owner_is_alive(owner: &LedgerOwner) -> bool {
    rebon_session_host::recorded_process_is_running(owner.pid, owner.pid_identity.as_deref())
        .unwrap_or(true)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Delivery {
    pub key: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobLedger {
    pub version: u32,
    /// The directory the starting server was confined to. A restarted server
    /// only takes over jobs started under its own root.
    pub root: String,
    pub owner: LedgerOwner,
    pub created_at_ms: u64,
    #[serde(default)]
    pub delivered: Vec<Delivery>,
}

impl JobLedger {
    pub(crate) fn was_delivered(&self, key: &str) -> bool {
        self.delivered.iter().any(|delivery| delivery.key == key)
    }
}

fn ledger_path(store: &BackgroundStore, job_id: &str) -> anyhow::Result<PathBuf> {
    // The id becomes a path component; this is the store's own rule for it.
    rebon_session_host::validate_job_id(job_id)?;
    Ok(store.job_dir(job_id).join(LEDGER_FILE))
}

fn lock(store: &BackgroundStore, job_id: &str) -> anyhow::Result<File> {
    rebon_session_host::validate_job_id(job_id)?;
    let path = store.job_dir(job_id).join(LEDGER_LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    FileExt::lock_exclusive(&file).with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(file)
}

fn read_unlocked(path: &Path) -> anyhow::Result<Option<JobLedger>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    let ledger = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(ledger))
}

fn write_unlocked(path: &Path, ledger: &JobLedger) -> anyhow::Result<()> {
    let payload = serde_json::to_string_pretty(ledger)?;
    rebon_session::write_file_atomically(path, format!("{payload}\n").as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Record that this surface started `job_id`, owned by `owner`.
pub(crate) fn create(
    store: &BackgroundStore,
    job_id: &str,
    root: &Path,
    owner: &LedgerOwner,
    now_ms: u64,
) -> anyhow::Result<()> {
    let path = ledger_path(store, job_id)?;
    let _lock = lock(store, job_id)?;
    write_unlocked(
        &path,
        &JobLedger {
            version: LEDGER_VERSION,
            root: root.to_string_lossy().into_owned(),
            owner: owner.clone(),
            created_at_ms: now_ms,
            delivered: Vec::new(),
        },
    )
}

/// The ledger for `job_id`, or `None` when the job was not started here.
pub(crate) fn read(store: &BackgroundStore, job_id: &str) -> anyhow::Result<Option<JobLedger>> {
    read_unlocked(&ledger_path(store, job_id)?)
}

/// Claim the right to push `key` for `job_id`. `true` exactly once per key,
/// across every server that will ever read this ledger.
///
/// Claimed before the push is written, not after: a server that dies between
/// the two loses that one push, which `job_status` still answers; the other
/// order would push it twice.
pub(crate) fn claim_delivery(
    store: &BackgroundStore,
    job_id: &str,
    key: &str,
    now_ms: u64,
) -> anyhow::Result<bool> {
    let path = ledger_path(store, job_id)?;
    let _lock = lock(store, job_id)?;
    let Some(mut ledger) = read_unlocked(&path)? else {
        return Ok(false);
    };
    if ledger.was_delivered(key) {
        return Ok(false);
    }
    ledger.delivered.push(Delivery {
        key: key.to_string(),
        at_ms: now_ms,
    });
    if ledger.delivered.len() > MAX_DELIVERIES {
        let excess = ledger.delivered.len() - MAX_DELIVERIES;
        ledger.delivered.drain(..excess);
    }
    write_unlocked(&path, &ledger)?;
    Ok(true)
}

/// Make `me` the owner of `job_id` if nobody else live is. `true` when `me`
/// owns it afterwards.
pub(crate) fn adopt(
    store: &BackgroundStore,
    job_id: &str,
    me: &LedgerOwner,
    alive: impl Fn(&LedgerOwner) -> bool,
) -> anyhow::Result<bool> {
    let path = ledger_path(store, job_id)?;
    let _lock = lock(store, job_id)?;
    let Some(mut ledger) = read_unlocked(&path)? else {
        return Ok(false);
    };
    if ledger.owner == *me {
        return Ok(true);
    }
    if alive(&ledger.owner) {
        return Ok(false);
    }
    ledger.owner = me.clone();
    write_unlocked(&path, &ledger)?;
    Ok(true)
}

/// Every job started under `root` within `window_ms` whose owner is gone,
/// now owned by `me`. This is how a restarted server picks its jobs back up —
/// and how one started after a crashed session learns what that session left
/// running.
pub(crate) fn adopt_orphans(
    store: &BackgroundStore,
    root: &Path,
    me: &LedgerOwner,
    now_ms: u64,
    window_ms: u64,
    alive: impl Fn(&LedgerOwner) -> bool,
) -> Vec<String> {
    let entries = match std::fs::read_dir(store.jobs_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(%error, "rebon mcp: could not list background jobs");
            return Vec::new();
        }
    };
    let root = root.to_string_lossy();
    let mut adopted = Vec::new();
    for entry in entries.flatten() {
        let job_id = entry.file_name().to_string_lossy().into_owned();
        // A name the store would refuse is not a job (the supervisor keeps
        // an event-only directory here too).
        if rebon_session_host::validate_job_id(&job_id).is_err() {
            continue;
        }
        let ledger = match read(store, &job_id) {
            Ok(Some(ledger)) => ledger,
            // No ledger: started somewhere else, not ours to push for.
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%job_id, %error, "rebon mcp: skipping an unreadable job ledger");
                continue;
            }
        };
        if !rebon_session::same_cwd(&ledger.root, &root)
            || now_ms.saturating_sub(ledger.created_at_ms) > window_ms
        {
            continue;
        }
        match adopt(store, &job_id, me, &alive) {
            Ok(true) => adopted.push(job_id),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%job_id, %error, "rebon mcp: could not take over a job");
            }
        }
    }
    adopted.sort();
    adopted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        (dir, store)
    }

    fn owner(pid: u32) -> LedgerOwner {
        LedgerOwner {
            pid,
            pid_identity: Some(format!("test:{pid}")),
        }
    }

    fn job_dir(store: &BackgroundStore, job_id: &str) {
        std::fs::create_dir_all(store.job_dir(job_id)).unwrap();
    }

    #[test]
    fn a_ledger_round_trips_and_marks_the_job_as_ours() {
        let (_dir, store) = store();
        job_dir(&store, "bg-1");
        assert_eq!(read(&store, "bg-1").unwrap(), None, "not started here");

        create(&store, "bg-1", Path::new("/proj"), &owner(10), 5).unwrap();
        let ledger = read(&store, "bg-1").unwrap().unwrap();
        assert_eq!(ledger.version, LEDGER_VERSION);
        assert_eq!(ledger.root, Path::new("/proj").to_string_lossy());
        assert_eq!(ledger.owner, owner(10));
        assert_eq!(ledger.created_at_ms, 5);
        assert!(ledger.delivered.is_empty());
    }

    #[test]
    fn a_key_is_claimed_once_even_by_a_fresh_reader() {
        let (_dir, store) = store();
        job_dir(&store, "bg-1");
        create(&store, "bg-1", Path::new("/proj"), &owner(10), 0).unwrap();

        assert!(claim_delivery(&store, "bg-1", "1:succeeded", 1).unwrap());
        assert!(!claim_delivery(&store, "bg-1", "1:succeeded", 2).unwrap());
        // A second server (or this one restarted) reads the same file.
        let reopened = BackgroundStore::new(store.root());
        assert!(!claim_delivery(&reopened, "bg-1", "1:succeeded", 3).unwrap());
        assert!(claim_delivery(&reopened, "bg-1", "2:succeeded", 4).unwrap());

        let ledger = read(&store, "bg-1").unwrap().unwrap();
        assert!(ledger.was_delivered("1:succeeded"));
        assert!(ledger.was_delivered("2:succeeded"));
        assert_eq!(ledger.delivered.len(), 2);
    }

    #[test]
    fn claiming_for_a_job_without_a_ledger_claims_nothing() {
        let (_dir, store) = store();
        job_dir(&store, "bg-x");
        assert!(!claim_delivery(&store, "bg-x", "1:failed", 0).unwrap());
        assert_eq!(read(&store, "bg-x").unwrap(), None, "no ledger conjured");
    }

    #[test]
    fn deliveries_are_capped_oldest_first() {
        let (_dir, store) = store();
        job_dir(&store, "bg-1");
        create(&store, "bg-1", Path::new("/proj"), &owner(10), 0).unwrap();
        for n in 0..(MAX_DELIVERIES + 3) {
            assert!(claim_delivery(&store, "bg-1", &format!("{n}:succeeded"), n as u64).unwrap());
        }
        let ledger = read(&store, "bg-1").unwrap().unwrap();
        assert_eq!(ledger.delivered.len(), MAX_DELIVERIES);
        assert!(!ledger.was_delivered("0:succeeded"));
        assert!(ledger.was_delivered(&format!("{}:succeeded", MAX_DELIVERIES + 2)));
    }

    #[test]
    fn a_live_owner_keeps_its_job_and_a_dead_one_hands_it_over() {
        let (_dir, store) = store();
        job_dir(&store, "bg-1");
        create(&store, "bg-1", Path::new("/proj"), &owner(10), 0).unwrap();
        let me = owner(20);

        assert!(!adopt(&store, "bg-1", &me, |_| true).unwrap());
        assert_eq!(read(&store, "bg-1").unwrap().unwrap().owner, owner(10));

        assert!(adopt(&store, "bg-1", &me, |_| false).unwrap());
        assert_eq!(read(&store, "bg-1").unwrap().unwrap().owner, me);
        assert!(
            adopt(&store, "bg-1", &me, |_| true).unwrap(),
            "the owner adopting its own job is a no-op yes"
        );
    }

    #[test]
    fn orphans_are_adopted_only_under_the_same_root_and_inside_the_window() {
        let (_dir, store) = store();
        let me = owner(99);
        for (job, root, created, dead) in [
            ("bg-orphan", "/proj", 1_000, true),
            ("bg-live-owner", "/proj", 1_000, false),
            ("bg-other-root", "/elsewhere", 1_000, true),
            ("bg-too-old", "/proj", 0, true),
        ] {
            job_dir(&store, job);
            let owner = if dead { owner(1) } else { owner(2) };
            create(&store, job, Path::new(root), &owner, created).unwrap();
        }
        // A job directory with no ledger, and something that is not a job.
        job_dir(&store, "bg-cli-only");
        std::fs::create_dir_all(store.jobs_dir().join("not a job id")).unwrap();

        let adopted = adopt_orphans(&store, Path::new("/proj"), &me, 2_000, 1_500, |o| {
            o.pid == 2
        });
        assert_eq!(adopted, vec!["bg-orphan".to_string()]);
        assert_eq!(read(&store, "bg-orphan").unwrap().unwrap().owner, me);
        assert_eq!(
            read(&store, "bg-live-owner").unwrap().unwrap().owner,
            owner(2)
        );
    }

    #[test]
    fn a_path_shaped_id_never_reaches_the_filesystem() {
        let (_dir, store) = store();
        for id in ["../escape", "bg/1", "", "bg 1", "..\\x"] {
            assert!(read(&store, id).is_err(), "{id:?}");
            assert!(claim_delivery(&store, id, "k", 0).is_err(), "{id:?}");
        }
    }

    #[test]
    fn no_jobs_directory_means_nothing_to_adopt() {
        let (_dir, store) = store();
        assert!(adopt_orphans(&store, Path::new("/proj"), &owner(1), 0, 10, |_| false).is_empty());
    }

    #[test]
    fn this_process_is_alive_and_a_finished_one_is_not() {
        assert!(owner_is_alive(&LedgerOwner::this_process()));
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit 0"])
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let pid = child.id();
        child.wait().unwrap();
        assert!(!owner_is_alive(&LedgerOwner {
            pid,
            pid_identity: Some("gone".into()),
        }));
        assert!(
            owner_is_alive(&LedgerOwner {
                pid: std::process::id(),
                pid_identity: None,
            }),
            "an owner that cannot be checked is not taken over"
        );
    }
}
