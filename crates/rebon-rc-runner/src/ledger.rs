//! `<config home>/rc/ledger.json`: what this machine did for RC.
//!
//! Two questions, both of which a restarted `rebon rc serve` has to be able
//! to answer:
//!
//! 1. **Which local session is this RC session?** A work item that names no
//!    resume target — from a server that predates `session_bound`, or
//!    queued before the binding reached it — still continues the session
//!    this machine already ran it as ([`Ledger::session`]).
//! 2. **Did this work item's prompt already run?** RC hands a leased item
//!    out again once its lease lapses, prompt and all. A runner that was
//!    restarted mid-session would otherwise run that prompt a second time.
//!    The prompt is claimed here *before* it is delivered
//!    ([`Ledger::claim_prompt`]): a crash between the two loses the prompt,
//!    which the controller can see and resend; the other order could run a
//!    command twice.
//!
//! Every read and write takes `ledger.lock`, and writes are atomic.

use std::collections::{BTreeMap, VecDeque};
use std::fs::OpenOptions;
use std::path::PathBuf;

use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::core::work::Remembered;
use crate::files::RcDir;

const LEDGER_VERSION: u32 = 1;
/// Sessions remembered. The oldest go first; a machine that served more
/// RC sessions than this still resumes the recent ones.
const MAX_SESSIONS: usize = 1024;
/// Prompt claims remembered. Only an item that is handed out again needs
/// one, and that happens within minutes of its lease lapsing.
const MAX_PROMPTS: usize = 512;

/// One RC session this machine ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    pub rebon_session_id: String,
    /// The project the work item named.
    pub project: String,
    /// Where the session's transcript lives (the project, or the worktree
    /// its job ran in).
    pub cwd: String,
    pub job_id: Option<String>,
    pub environment_id: String,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptClaim {
    work_id: String,
    at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerFile {
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, SessionEntry>,
    #[serde(default)]
    prompts: VecDeque<PromptClaim>,
}

impl Default for LedgerFile {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            sessions: BTreeMap::new(),
            prompts: VecDeque::new(),
        }
    }
}

/// The ledger, shared by every work item of one `serve`.
#[derive(Debug, Clone)]
pub struct Ledger {
    dir: RcDir,
    max_sessions: usize,
    max_prompts: usize,
}

impl Ledger {
    pub fn new(dir: RcDir) -> Self {
        Self {
            dir,
            max_sessions: MAX_SESSIONS,
            max_prompts: MAX_PROMPTS,
        }
    }

    /// A ledger with smaller bounds, so a test can reach them without
    /// thousands of file writes.
    #[cfg(test)]
    fn with_bounds(dir: RcDir, max_sessions: usize, max_prompts: usize) -> Self {
        Self {
            dir,
            max_sessions,
            max_prompts,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.ledger_path()
    }

    /// The local session `rc_session_id` ran as, if this machine ran it.
    pub fn session(&self, rc_session_id: &str) -> anyhow::Result<Option<SessionEntry>> {
        self.with(|file| Ok((file.sessions.get(rc_session_id).cloned(), false)))
    }

    /// [`Self::session`] in the shape the work planner asks for. An
    /// unreadable ledger is logged and treated as empty: the item then
    /// names its own target or starts fresh, and nothing runs twice
    /// because of it.
    pub fn remembered(&self, rc_session_id: &str) -> Option<Remembered> {
        match self.session(rc_session_id) {
            Ok(entry) => entry.map(|entry| Remembered {
                rebon_session_id: entry.rebon_session_id,
                project: entry.project,
            }),
            Err(error) => {
                tracing::warn!(%error, "rebon rc: could not read the ledger");
                None
            }
        }
    }

    pub fn sessions(&self) -> anyhow::Result<BTreeMap<String, SessionEntry>> {
        self.with(|file| Ok((file.sessions.clone(), false)))
    }

    pub fn record_session(&self, rc_session_id: &str, entry: SessionEntry) -> anyhow::Result<()> {
        let max_sessions = self.max_sessions;
        self.with(|file| {
            file.sessions.insert(rc_session_id.to_string(), entry);
            while file.sessions.len() > max_sessions {
                let oldest = file
                    .sessions
                    .iter()
                    .min_by_key(|(_, entry)| entry.updated_at_ms)
                    .map(|(key, _)| key.clone())
                    .expect("the map is not empty");
                file.sessions.remove(&oldest);
            }
            Ok(((), true))
        })
    }

    /// `true` exactly once per work id: the caller may deliver its prompt.
    pub fn claim_prompt(&self, work_id: &str, now_ms: u64) -> anyhow::Result<bool> {
        let max_prompts = self.max_prompts;
        self.with(|file| {
            if file.prompts.iter().any(|claim| claim.work_id == work_id) {
                return Ok((false, false));
            }
            file.prompts.push_back(PromptClaim {
                work_id: work_id.to_string(),
                at_ms: now_ms,
            });
            while file.prompts.len() > max_prompts {
                file.prompts.pop_front();
            }
            Ok((true, true))
        })
    }

    /// Run `action` on the ledger under its lock; write it back when the
    /// action says it changed something.
    fn with<T>(
        &self,
        action: impl FnOnce(&mut LedgerFile) -> anyhow::Result<(T, bool)>,
    ) -> anyhow::Result<T> {
        self.dir.ensure()?;
        let lock_path = self.dir.ledger_lock_path();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("failed to lock {}", lock_path.display()))?;
        let path = self.path();
        let mut file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<LedgerFile>(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LedgerFile::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        if file.version > LEDGER_VERSION {
            anyhow::bail!(
                "{} was written by a newer Rebon (version {}); refusing to rewrite it",
                path.display(),
                file.version
            );
        }
        let (value, changed) = action(&mut file)?;
        if changed {
            file.version = LEDGER_VERSION;
            let payload = serde_json::to_vec_pretty(&file)?;
            rebon_session::write_file_atomically(&path, &payload)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, at: u64) -> SessionEntry {
        SessionEntry {
            rebon_session_id: id.into(),
            project: "/srv/app".into(),
            cwd: "/srv/app".into(),
            job_id: Some("bg-1".into()),
            environment_id: "env_1".into(),
            updated_at_ms: at,
        }
    }

    fn ledger() -> (tempfile::TempDir, Ledger) {
        let home = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(RcDir::new(home.path()));
        (home, ledger)
    }

    #[test]
    fn a_session_is_remembered_across_instances() {
        let (home, ledger) = ledger();
        assert!(ledger.session("sess_1").unwrap().is_none());
        assert!(ledger.remembered("sess_1").is_none());
        ledger
            .record_session("sess_1", entry("local-1", 1))
            .unwrap();
        // A second process (or a restarted one) reads the same file.
        let again = Ledger::new(RcDir::new(home.path()));
        assert_eq!(again.session("sess_1").unwrap(), Some(entry("local-1", 1)));
        assert_eq!(
            again.remembered("sess_1"),
            Some(Remembered {
                rebon_session_id: "local-1".into(),
                project: "/srv/app".into()
            })
        );
        // A later record replaces it.
        again.record_session("sess_1", entry("local-2", 2)).unwrap();
        assert_eq!(
            ledger.session("sess_1").unwrap().unwrap().rebon_session_id,
            "local-2"
        );
        assert_eq!(ledger.sessions().unwrap().len(), 1);
    }

    #[test]
    fn a_prompt_is_claimed_once_even_after_a_restart() {
        let (home, ledger) = ledger();
        assert!(ledger.claim_prompt("wrk_1", 1).unwrap());
        assert!(!ledger.claim_prompt("wrk_1", 2).unwrap());
        let restarted = Ledger::new(RcDir::new(home.path()));
        assert!(!restarted.claim_prompt("wrk_1", 3).unwrap());
        assert!(restarted.claim_prompt("wrk_2", 3).unwrap());
    }

    #[test]
    fn the_ledger_stays_bounded() {
        let home = tempfile::tempdir().unwrap();
        let ledger = Ledger::with_bounds(RcDir::new(home.path()), 5, 4);
        for index in 0..(4 + 10) {
            assert!(ledger.claim_prompt(&format!("wrk_{index}"), 1).unwrap());
        }
        // The oldest claims fell off; the newest are still there.
        assert!(ledger.claim_prompt("wrk_0", 2).unwrap());
        assert!(!ledger.claim_prompt(&format!("wrk_{}", 4 + 9), 2).unwrap());

        for index in 0..(5 + 3) {
            ledger
                .record_session(&format!("sess_{index}"), entry("l", index as u64))
                .unwrap();
        }
        let sessions = ledger.sessions().unwrap();
        assert_eq!(sessions.len(), 5);
        assert!(!sessions.contains_key("sess_0"));
        assert!(sessions.contains_key(&format!("sess_{}", 5 + 2)));
        // The production bounds are what the docs say.
        let defaults = Ledger::new(RcDir::new(home.path()));
        assert_eq!(
            (defaults.max_sessions, defaults.max_prompts),
            (MAX_SESSIONS, MAX_PROMPTS)
        );
    }

    #[test]
    fn a_corrupt_or_newer_ledger_is_not_overwritten() {
        let (_home, ledger) = ledger();
        ledger.dir.ensure().unwrap();
        std::fs::write(ledger.path(), b"{broken").unwrap();
        assert!(ledger.claim_prompt("wrk_1", 1).is_err());
        assert!(ledger.remembered("sess_1").is_none());
        assert_eq!(std::fs::read(ledger.path()).unwrap(), b"{broken");

        std::fs::write(ledger.path(), br#"{"version": 99}"#).unwrap();
        let error = ledger.record_session("s", entry("l", 1)).unwrap_err();
        assert!(error.to_string().contains("newer"), "{error}");
    }
}
