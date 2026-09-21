//! The ACE ledger.
//!
//! ## Why a ledger exists at all
//!
//! An ACE is not a sandbox artefact that disappears with the process. It is a
//! persistent modification to the user's real disk. If Rebon is killed with deny
//! ACEs outstanding, the user's own editor stops being able to open files in
//! their own project, and nothing on the machine says why. The ledger is the
//! only thing that can put that back.
//!
//! So this is not an optimisation and it is not a cache; it is the recovery
//! record. That is also why it is JSON written through a temp file and an atomic
//! rename rather than SQLite: a few dozen append-only rows need no query engine,
//! and for security-critical state that a person may one day have to read by
//! hand, auditability beats convenience.
//!
//! ## Identity is the file, not the path
//!
//! Every entry stores a [`FileId`] — volume serial plus file index, from
//! `GetFileInformationByHandle`. Revocation reopens the path and compares. A
//! path can be renamed while nobody is looking, and revoking by path would then
//! strip an ACE from a completely different file. On a mismatch the answer is to
//! refuse and say so, never to guess.
//!
//! Two more cases refuse rather than act:
//!
//! * **Hard links** (`nNumberOfLinks > 1`): an ACE placed through one path
//!   applies to every name the file has, so "revoke this one" has no single
//!   meaning.
//! * **Another session still holds it**: two sessions that denied the same path
//!   each own a row, and the ACE may only come off when the last one goes.
//!   Without this, the first session to exit unconfines the second.

use crate::core::acl::AceKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Schema version of the on-disk ledger.
pub const LEDGER_VERSION: u32 = 1;

/// The file the ledger lives in, under `%LOCALAPPDATA%\Rebon\sandbox-win\`.
pub const LEDGER_FILE_NAME: &str = "ledger.json";

/// A file's identity, independent of its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    /// The volume serial number.
    pub volume_serial: u32,
    /// The file index, high and low halves joined.
    pub index: u64,
}

/// The process whose death makes an entry reapable.
///
/// The creation time is not decoration: PIDs are reused, and a reaper that
/// matched on PID alone would eventually decide a live session is dead — or,
/// worse, that a dead one is alive and leave its ACEs on the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    /// Process creation time, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
}

impl Owner {
    /// The session these ACEs belong to.
    ///
    /// ACEs are a **session-level** resource: the caller refuses per-command
    /// `allowRead`/`allowWrite`, so every `exec` from one Rebon process asks for the
    /// same set. But the helper is a per-command process — it exits when the command
    /// does — so it cannot own anything that has to outlive one command. The owner
    /// is the process that *called* it, and the session id is derived from that
    /// owner rather than passed in: the `exec` grammar is frozen and has no field
    /// for one.
    ///
    /// The creation time is in the id, not just the PID. Two Rebon processes where
    /// the second reused the first's PID must not inherit each other's rows — the
    /// second would revoke ACEs it never placed, and the first's would never be
    /// reaped.
    pub fn session_id(&self) -> String {
        format!("{}-{}", self.pid, self.started_at_ms)
    }
}

/// One ACE the helper placed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: String,
    /// For humans reading the file. Never used to decide anything.
    pub path: String,
    pub file_id: FileId,
    pub kind: AceKind,
    pub trustee_sid: String,
    pub session_id: String,
    pub owner: Owner,
    /// Directories created because the target did not exist, outermost first — the
    /// placeholder chain. Removed, innermost first, when the entry is revoked.
    #[serde(default)]
    pub placeholders: Vec<String>,
    pub placed_at_ms: u64,
}

impl LedgerEntry {
    /// What makes two rows the same ACE. Session and id are excluded on purpose: two
    /// sessions denying one path are two rows for one ACE.
    fn ace_key(&self) -> (&str, AceKind, u32, u64) {
        (
            self.trustee_sid.as_str(),
            self.kind,
            self.file_id.volume_serial,
            self.file_id.index,
        )
    }
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<LedgerEntry>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Why a ledger could not be read.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("the ledger is not valid JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(
        "the ledger is version {found}, this sandbox-win understands {LEDGER_VERSION} — \
         run `sandbox-win.exe uninstall` with the matching version, or remove the ACEs by hand"
    )]
    UnknownVersion { found: u32 },
}

/// What [`Ledger::record`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// A new row; the ACE has to be written.
    Added,
    /// This session already recorded this ACE. Re-entry after a crash lands here,
    /// and writing the ACE again is harmless but recording it twice is not — the
    /// second revoke would find nothing and report a mismatch.
    AlreadyRecorded,
}

/// What was found when a path was reopened for revocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// The path no longer resolves.
    Missing,
    Present {
        file_id: FileId,
        links: u32,
    },
}

/// Whether an entry's ACE may be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeDecision {
    /// Remove the ACE, then drop the row.
    Revoke,
    /// Drop the row without touching the disk.
    DropWithoutRevoking { reason: String },
    /// Leave both alone and tell the user which path.
    Refuse { reason: String },
}

impl Ledger {
    pub fn load(bytes: &[u8]) -> Result<Self, LedgerError> {
        // An empty file is a ledger that was created and never written to, which is the
        // same thing as no ACEs.
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(Self::default());
        }
        let ledger: Ledger = serde_json::from_slice(bytes)?;
        if ledger.version != LEDGER_VERSION {
            return Err(LedgerError::UnknownVersion {
                found: ledger.version,
            });
        }
        Ok(ledger)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        // Pretty-printed: a person recovering a machine by hand is a supported path,
        // and this file is small.
        let mut bytes = serde_json::to_vec_pretty(self).unwrap_or_else(|_| b"{}".to_vec());
        bytes.push(b'\n');
        bytes
    }

    /// Record an ACE this session is placing.
    pub fn record(&mut self, entry: LedgerEntry) -> RecordOutcome {
        let already = self.entries.iter().any(|existing| {
            existing.session_id == entry.session_id && existing.ace_key() == entry.ace_key()
        });
        if already {
            return RecordOutcome::AlreadyRecorded;
        }
        self.entries.push(entry);
        RecordOutcome::Added
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    pub fn get(&self, id: &str) -> Option<&LedgerEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// How many *other* rows claim the same ACE.
    pub fn other_claims(&self, entry: &LedgerEntry) -> usize {
        self.entries
            .iter()
            .filter(|existing| existing.id != entry.id && existing.ace_key() == entry.ace_key())
            .count()
    }

    /// Rows belonging to `session_id`, in the order they were placed.
    pub fn entries_for_session(&self, session_id: &str) -> Vec<&LedgerEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.session_id == session_id)
            .collect()
    }

    /// Rows whose owning process is gone.
    ///
    /// `exec` runs this before it does anything else, so a session that was killed
    /// cleans up on the next command rather than on the next reboot.
    pub fn reap_targets(&self, is_alive: impl Fn(&Owner) -> bool) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| !is_alive(&entry.owner))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Revocation order: newest first.
    ///
    /// ACEs nest — a deny inside a write root is placed after the grant that
    /// contains it — and undoing a nest from the inside out is the order that never
    /// leaves a placeholder directory stranded under a removed parent.
    pub fn revocation_order(&self, ids: &[String]) -> Vec<String> {
        let mut selected: Vec<&LedgerEntry> = self
            .entries
            .iter()
            .filter(|entry| ids.iter().any(|id| id == &entry.id))
            .collect();
        selected.sort_by(|a, b| b.placed_at_ms.cmp(&a.placed_at_ms).then(b.id.cmp(&a.id)));
        selected.into_iter().map(|entry| entry.id.clone()).collect()
    }
}

/// Whether one entry's ACE may be revoked, given what the disk says now.
///
/// `other_claims` comes from [`Ledger::other_claims`]; it is a parameter rather
/// than a lookup so this stays a pure function of the three inputs that decide
/// the answer.
pub fn revoke_decision(
    entry: &LedgerEntry,
    observed: &Observation,
    other_claims: usize,
) -> RevokeDecision {
    if other_claims > 0 {
        return RevokeDecision::DropWithoutRevoking {
            reason: format!(
                "{} other session(s) still hold this ACE on {}",
                other_claims, entry.path
            ),
        };
    }

    match observed {
        // The file is gone and so is its security descriptor. Keeping the row would
        // make every later reap re-report the same phantom.
        Observation::Missing => RevokeDecision::DropWithoutRevoking {
            reason: format!("{} no longer exists", entry.path),
        },
        Observation::Present { file_id, links } if *file_id != entry.file_id => {
            RevokeDecision::Refuse {
                reason: format!(
                    "{} is not the file the ACE was placed on — it was replaced or renamed; \
                     remove the ACE by hand",
                    entry.path
                ),
            }
        }
        Observation::Present { links, .. } if *links > 1 => RevokeDecision::Refuse {
            reason: format!(
                "{} has {links} hard links, so an ACE placed through one name applies to all \
                 of them and revoking through this one is ambiguous; remove it by hand",
                entry.path
            ),
        },
        Observation::Present { .. } => RevokeDecision::Revoke,
    }
}

/// The directories that must be created so `path` exists.
///
/// Outermost first, so creation runs front to back and removal runs back to
/// front. The chain is recorded because it closes a substitution: delete the
/// denied target, let the sandbox recreate a directory of the same name, and the
/// deny ACE is gone while the rule still looks like it is in force.
pub fn missing_ancestors(path: &Path, exists: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(candidate) = current {
        if exists(candidate) {
            break;
        }
        missing.push(candidate.to_path_buf());
        current = candidate
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
    }
    missing.reverse();
    missing
}

/// The order placeholder directories come back out in: innermost first.
pub fn placeholder_removal_order(created: &[String]) -> Vec<PathBuf> {
    created.iter().rev().map(PathBuf::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_id(index: u64) -> FileId {
        FileId {
            volume_serial: 0x1234_5678,
            index,
        }
    }

    fn entry(id: &str, path: &str, index: u64, session: &str) -> LedgerEntry {
        LedgerEntry {
            id: id.to_string(),
            path: path.to_string(),
            file_id: file_id(index),
            kind: AceKind::DenyWrite,
            trustee_sid: "S-1-5-21-1-2-3-1004".to_string(),
            session_id: session.to_string(),
            owner: Owner {
                pid: 4242,
                started_at_ms: 1_756_000_000_000,
            },
            placeholders: Vec::new(),
            placed_at_ms: 1_756_000_000_000,
        }
    }

    fn present(index: u64) -> Observation {
        Observation::Present {
            file_id: file_id(index),
            links: 1,
        }
    }

    #[test]
    fn a_ledger_round_trips_through_json() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1"));

        let reloaded = Ledger::load(&ledger.to_bytes()).unwrap();

        assert_eq!(reloaded, ledger);
    }

    #[test]
    fn an_empty_file_loads_as_an_empty_ledger() {
        // The file is created before the first ACE is placed; a helper that refused to
        // start on it would be unable to place the first one.
        assert_eq!(Ledger::load(b"").unwrap(), Ledger::default());
        assert_eq!(Ledger::load(b"  \n").unwrap(), Ledger::default());
    }

    #[test]
    fn a_corrupt_ledger_is_an_error_not_an_empty_one() {
        // Treating unreadable state as "no ACEs" would silently abandon every ACE on
        // the disk, which is the exact outcome the ledger exists to prevent.
        assert!(matches!(
            Ledger::load(b"{ not json").unwrap_err(),
            LedgerError::Malformed(_)
        ));
    }

    #[test]
    fn a_future_version_is_refused_with_a_way_out() {
        let future = br#"{"version": 99, "entries": []}"#;
        let error = Ledger::load(future).unwrap_err();
        assert!(matches!(error, LedgerError::UnknownVersion { found: 99 }));
        assert!(error.to_string().contains("uninstall"));
    }

    #[test]
    fn recording_the_same_ace_twice_in_one_session_makes_one_row() {
        // Crash re-entry: the helper died between writing the ACE and flushing the
        // ledger, and the next run places it again.
        let mut ledger = Ledger::default();
        assert_eq!(
            ledger.record(entry("a", r"C:\x", 10, "s1")),
            RecordOutcome::Added
        );
        assert_eq!(
            ledger.record(entry("b", r"C:\x", 10, "s1")),
            RecordOutcome::AlreadyRecorded
        );
        assert_eq!(ledger.entries.len(), 1);
    }

    #[test]
    fn two_sessions_denying_one_path_each_get_a_row() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\x", 10, "s1"));
        assert_eq!(
            ledger.record(entry("b", r"C:\x", 10, "s2")),
            RecordOutcome::Added
        );
        assert_eq!(ledger.entries.len(), 2);
    }

    #[test]
    fn the_first_session_to_exit_does_not_unconfine_the_second() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\x", 10, "s1"));
        ledger.record(entry("b", r"C:\x", 10, "s2"));

        let first = ledger.get("a").unwrap().clone();
        let decision = revoke_decision(&first, &present(10), ledger.other_claims(&first));

        assert!(matches!(
            decision,
            RevokeDecision::DropWithoutRevoking { .. }
        ));
    }

    #[test]
    fn the_last_session_to_exit_does_revoke() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\x", 10, "s1"));
        ledger.record(entry("b", r"C:\x", 10, "s2"));
        ledger.remove("a");

        let last = ledger.get("b").unwrap().clone();

        assert_eq!(
            revoke_decision(&last, &present(10), ledger.other_claims(&last)),
            RevokeDecision::Revoke
        );
    }

    #[test]
    fn a_renamed_path_refuses_rather_than_stripping_another_files_ace() {
        let row = entry("a", r"C:\work\vendor", 10, "s1");
        let decision = revoke_decision(&row, &present(999), 0);

        match decision {
            RevokeDecision::Refuse { reason } => {
                assert!(reason.contains(r"C:\work\vendor"), "{reason}");
                assert!(reason.contains("by hand"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_different_volume_is_a_different_file_even_at_the_same_index() {
        let row = entry("a", r"C:\x", 10, "s1");
        let moved = Observation::Present {
            file_id: FileId {
                volume_serial: 0xDEAD_BEEF,
                index: 10,
            },
            links: 1,
        };
        assert!(matches!(
            revoke_decision(&row, &moved, 0),
            RevokeDecision::Refuse { .. }
        ));
    }

    #[test]
    fn a_hard_linked_file_refuses_and_names_the_path() {
        let row = entry("a", r"C:\work\shared.txt", 10, "s1");
        let linked = Observation::Present {
            file_id: file_id(10),
            links: 3,
        };

        match revoke_decision(&row, &linked, 0) {
            RevokeDecision::Refuse { reason } => {
                assert!(reason.contains("hard links"), "{reason}");
                assert!(reason.contains(r"C:\work\shared.txt"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_vanished_path_drops_the_row_without_touching_the_disk() {
        let row = entry("a", r"C:\gone", 10, "s1");
        assert!(matches!(
            revoke_decision(&row, &Observation::Missing, 0),
            RevokeDecision::DropWithoutRevoking { .. }
        ));
    }

    #[test]
    fn a_shared_claim_wins_over_a_mismatch_because_the_disk_is_not_ours_to_judge() {
        // Another session still holds the ACE, so this row must come out either way —
        // reporting a mismatch on an ACE we are not removing would be noise the user
        // cannot act on.
        let row = entry("a", r"C:\x", 10, "s1");
        assert!(matches!(
            revoke_decision(&row, &present(999), 1),
            RevokeDecision::DropWithoutRevoking { .. }
        ));
    }

    #[test]
    fn reaping_selects_exactly_the_dead_owners() {
        let mut ledger = Ledger::default();
        let mut live = entry("live", r"C:\a", 10, "s1");
        live.owner.pid = 1;
        let mut dead = entry("dead", r"C:\b", 11, "s2");
        dead.owner.pid = 2;
        ledger.record(live);
        ledger.record(dead);

        let targets = ledger.reap_targets(|owner| owner.pid == 1);

        assert_eq!(targets, vec!["dead".to_string()]);
    }

    #[test]
    fn a_recycled_pid_does_not_look_alive() {
        // The whole reason `started_at_ms` is in the record: a new process has the old
        // one's PID, and a PID-only check would keep the dead session's ACEs on the disk
        // forever.
        let mut ledger = Ledger::default();
        let mut row = entry("a", r"C:\a", 10, "s1");
        row.owner = Owner {
            pid: 4242,
            started_at_ms: 1_000,
        };
        ledger.record(row);

        let alive_now = Owner {
            pid: 4242,
            started_at_ms: 2_000,
        };
        let targets = ledger.reap_targets(|owner| *owner == alive_now);

        assert_eq!(targets, vec!["a".to_string()]);
    }

    #[test]
    fn revocation_runs_newest_first() {
        let mut ledger = Ledger::default();
        let mut outer = entry("outer", r"C:\work", 10, "s1");
        outer.kind = AceKind::AllowWrite;
        outer.placed_at_ms = 100;
        let mut inner = entry("inner", r"C:\work\vendor", 11, "s1");
        inner.placed_at_ms = 200;
        ledger.record(outer);
        ledger.record(inner);

        let order = ledger.revocation_order(&["outer".into(), "inner".into()]);

        assert_eq!(order, vec!["inner".to_string(), "outer".to_string()]);
    }

    #[test]
    fn revocation_order_is_stable_when_timestamps_tie() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\a", 10, "s1"));
        ledger.record(entry("b", r"C:\b", 11, "s1"));

        let first = ledger.revocation_order(&["a".into(), "b".into()]);
        let second = ledger.revocation_order(&["b".into(), "a".into()]);

        assert_eq!(first, second);
    }

    #[test]
    fn removing_a_row_reports_whether_it_was_there() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\a", 10, "s1"));
        assert!(ledger.remove("a"));
        assert!(!ledger.remove("a"));
    }

    #[test]
    fn entries_are_selectable_by_session() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\a", 10, "s1"));
        ledger.record(entry("b", r"C:\b", 11, "s2"));

        assert_eq!(ledger.entries_for_session("s1").len(), 1);
        assert_eq!(ledger.entries_for_session("nope").len(), 0);
    }

    /// A root the running platform's own path parser will actually split.
    ///
    /// The rest of this module uses `C:\…` literals, which is fine there: they are
    /// opaque keys. The placeholder chain is different — it is walked with
    /// `Path::parent()`, and that splits only on the separator of the platform the
    /// test is running on. A Windows literal is one indivisible component on a
    /// non-Windows runner, so a chain test written with one collapses to a single
    /// entry and passes only on Windows. Production only ever sees Windows paths;
    /// the walk being tested is the same either way.
    fn native_root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\work" } else { "/work" })
    }

    #[test]
    fn the_placeholder_chain_is_outermost_first() {
        let existing = native_root();
        let chain = missing_ancestors(&existing.join("a").join("b").join("c"), |path| {
            path == existing
        });

        assert_eq!(
            chain,
            vec![
                existing.join("a"),
                existing.join("a").join("b"),
                existing.join("a").join("b").join("c"),
            ]
        );
    }

    #[test]
    fn an_existing_path_needs_no_placeholders() {
        assert!(missing_ancestors(Path::new(r"C:\work"), |_| true).is_empty());
    }

    #[test]
    fn placeholders_come_out_innermost_first() {
        let created = vec![
            r"C:\work\a".to_string(),
            r"C:\work\a\b".to_string(),
            r"C:\work\a\b\c".to_string(),
        ];
        assert_eq!(
            placeholder_removal_order(&created),
            vec![
                PathBuf::from(r"C:\work\a\b\c"),
                PathBuf::from(r"C:\work\a\b"),
                PathBuf::from(r"C:\work\a"),
            ]
        );
    }

    #[test]
    fn placeholders_survive_a_round_trip_through_json() {
        let mut row = entry("a", r"C:\work\a\b", 10, "s1");
        row.placeholders = vec![r"C:\work\a".into(), r"C:\work\a\b".into()];
        let mut ledger = Ledger::default();
        ledger.record(row.clone());

        let reloaded = Ledger::load(&ledger.to_bytes()).unwrap();

        assert_eq!(reloaded.get("a").unwrap().placeholders, row.placeholders);
    }

    #[test]
    fn a_row_written_before_placeholders_existed_still_loads() {
        let older = br#"{
          "version": 1,
          "entries": [{
            "id": "a",
            "path": "C:\\x",
            "file_id": {"volume_serial": 1, "index": 2},
            "kind": "deny_write",
            "trustee_sid": "S-1-5-21-1-2-3-1004",
            "session_id": "s1",
            "owner": {"pid": 1, "started_at_ms": 2},
            "placed_at_ms": 3
          }]
        }"#;
        let ledger = Ledger::load(older).unwrap();
        assert!(ledger.get("a").unwrap().placeholders.is_empty());
    }

    #[test]
    fn the_ace_kind_serialises_as_the_name_in_the_rfc() {
        let mut ledger = Ledger::default();
        let mut row = entry("a", r"C:\x", 10, "s1");
        row.kind = AceKind::AllowWrite;
        ledger.record(row);

        let text = String::from_utf8(ledger.to_bytes()).unwrap();

        assert!(text.contains(r#""kind": "allow_write""#), "{text}");
    }
}
