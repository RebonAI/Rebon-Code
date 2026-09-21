//! Undoing the ACEs of sessions that are gone.
//!
//! Every `exec` reaps before it does anything else. That is the difference
//! between "Rebon was killed" and "the user's editor cannot open files in their
//! own project and nothing on the machine explains why": ACEs are a persistent
//! modification to a real disk, and the process that knew about them is not
//! coming back to tidy up.
//!
//! ## Everything here is injected
//!
//! Liveness, file observation, and the ACE writer all arrive as parameters. That
//! is not testing ceremony — this is the code that decides whether to change the
//! permissions on a file the user owns, and it needs a coverage matrix rather
//! than a happy path. Injection is what lets that matrix run on any machine,
//! including the ones where the Win32 half does not exist.
//!
//! ## The ordering rule
//!
//! Revocation runs newest-first
//! ([`crate::core::ledger::Ledger::revocation_order`]). ACEs nest — a deny inside
//! a write root is placed after the grant that contains it — and unwinding a nest
//! from the inside out is the order that never strands a placeholder directory
//! under a parent that has already gone.
//!
//! ## Failures do not remove rows
//!
//! If revoking fails, or the file is not the one the ledger recorded, the row
//! **stays**. A row is the only record that an ACE is on the disk; dropping it
//! because the removal failed would convert a recoverable problem into a
//! permanent one.

use crate::core::ledger::{
    placeholder_removal_order, revoke_decision, Ledger, LedgerEntry, Observation, Owner,
    RevokeDecision,
};
use crate::sys::acl::AceWriter;
use crate::sys::SysResult;
use std::path::{Path, PathBuf};

/// What a reap did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// ACEs removed from the disk.
    pub revoked: Vec<String>,
    /// Rows dropped without touching the disk — the file was already gone,
    /// or another session still holds the ACE.
    pub dropped: Vec<(String, String)>,
    /// Rows left alone because acting would have been a guess. These need a
    /// person: `(path, why)`.
    pub refused: Vec<(String, String)>,
    /// Rows left alone because the removal itself failed: `(path, why)`.
    pub failed: Vec<(String, String)>,
    /// Placeholder directories removed.
    pub placeholders_removed: Vec<PathBuf>,
}

impl ReapReport {
    pub fn is_empty(&self) -> bool {
        self.revoked.is_empty()
            && self.dropped.is_empty()
            && self.refused.is_empty()
            && self.failed.is_empty()
    }

    /// Whether anything needs a person to look at it.
    pub fn needs_attention(&self) -> bool {
        !self.refused.is_empty() || !self.failed.is_empty()
    }
}

/// Everything the reaper needs from the outside world.
pub struct ReapContext<'a> {
    pub writer: &'a dyn AceWriter,
    /// Whether a recorded owner is still running.
    pub is_alive: &'a dyn Fn(&Owner) -> bool,
    /// What the disk says about a path now.
    pub observe: &'a dyn Fn(&Path) -> SysResult<Observation>,
    /// Remove an empty placeholder directory. Failure is not fatal.
    pub remove_directory: &'a dyn Fn(&Path) -> SysResult<()>,
}

/// Revoke everything belonging to sessions that are no longer running.
pub fn reap_dead_sessions(ledger: &mut Ledger, context: &ReapContext<'_>) -> ReapReport {
    let targets = ledger.reap_targets(context.is_alive);
    revoke_entries(ledger, &targets, context)
}

/// Revoke everything belonging to one session — the end-of-session path.
pub fn revoke_session(
    ledger: &mut Ledger,
    session_id: &str,
    context: &ReapContext<'_>,
) -> ReapReport {
    let targets: Vec<String> = ledger
        .entries_for_session(session_id)
        .into_iter()
        .map(|entry| entry.id.clone())
        .collect();
    revoke_entries(ledger, &targets, context)
}

/// Revoke a named set of rows.
pub fn revoke_entries(
    ledger: &mut Ledger,
    ids: &[String],
    context: &ReapContext<'_>,
) -> ReapReport {
    let mut report = ReapReport::default();

    for id in ledger.revocation_order(ids) {
        let Some(entry) = ledger.get(&id).cloned() else {
            continue;
        };

        // Recomputed inside the loop, not once up front: revoking one row
        // changes how many others claim the same ACE, and a stale count would
        // leave the last holder's ACE on the disk.
        let others = ledger.other_claims(&entry);

        let observed = match (context.observe)(Path::new(&entry.path)) {
            Ok(observed) => observed,
            Err(error) => {
                report.failed.push((entry.path.clone(), error.to_string()));
                continue;
            }
        };

        match revoke_decision(&entry, &observed, others) {
            RevokeDecision::Revoke => {
                match context
                    .writer
                    .revoke(Path::new(&entry.path), entry.kind, &entry.trustee_sid)
                {
                    Ok(()) => {
                        clean_placeholders(&entry, context, &mut report);
                        ledger.remove(&entry.id);
                        report.revoked.push(entry.path.clone());
                    }
                    // The row stays. It is the only thing that knows this ACE
                    // is on the user's disk.
                    Err(error) => report.failed.push((entry.path.clone(), error.to_string())),
                }
            }
            RevokeDecision::DropWithoutRevoking { reason } => {
                // Nothing on disk to undo, but the placeholders we created
                // are still ours to remove when no one else claims them.
                if others == 0 {
                    clean_placeholders(&entry, context, &mut report);
                }
                ledger.remove(&entry.id);
                report.dropped.push((entry.path.clone(), reason));
            }
            RevokeDecision::Refuse { reason } => {
                report.refused.push((entry.path.clone(), reason));
            }
        }
    }

    report
}

/// Remove the directories this entry created, innermost first.
///
/// Best effort, and deliberately so: a placeholder that has since been filled
/// with real files is not ours to delete, and the OS refusing to remove a
/// non-empty directory is exactly the check we want.
fn clean_placeholders(entry: &LedgerEntry, context: &ReapContext<'_>, report: &mut ReapReport) {
    for directory in placeholder_removal_order(&entry.placeholders) {
        if (context.remove_directory)(&directory).is_ok() {
            report.placeholders_removed.push(directory);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::acl::{AceKind, PlannedAce};
    use crate::core::ledger::{FileId, LedgerEntry};
    use crate::sys::{SysError, SysResult};
    use std::cell::RefCell;
    use std::collections::HashSet;

    const TRUSTEE: &str = "S-1-5-21-1-2-3-1004";

    #[derive(Default)]
    struct FakeWriter {
        revoked: RefCell<Vec<String>>,
        fail_on: HashSet<String>,
    }

    impl AceWriter for FakeWriter {
        fn place(&self, _path: &Path, _ace: &PlannedAce, _trustee: &str) -> SysResult<()> {
            Ok(())
        }

        fn revoke(&self, path: &Path, _kind: AceKind, _trustee: &str) -> SysResult<()> {
            let path = path.to_string_lossy().into_owned();
            if self.fail_on.contains(&path) {
                return Err(SysError::Invalid("the DACL could not be written".into()));
            }
            self.revoked.borrow_mut().push(path);
            Ok(())
        }
    }

    fn entry(id: &str, path: &str, index: u64, session: &str, pid: u32) -> LedgerEntry {
        LedgerEntry {
            id: id.to_string(),
            path: path.to_string(),
            file_id: FileId {
                volume_serial: 7,
                index,
            },
            kind: AceKind::DenyWrite,
            trustee_sid: TRUSTEE.into(),
            session_id: session.to_string(),
            owner: Owner {
                pid,
                started_at_ms: 1_000,
            },
            placeholders: Vec::new(),
            placed_at_ms: index,
        }
    }

    struct Harness {
        writer: FakeWriter,
        present: Vec<(String, u64, u32)>,
        alive: HashSet<u32>,
        removed: RefCell<Vec<PathBuf>>,
        removable: HashSet<String>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                writer: FakeWriter::default(),
                present: Vec::new(),
                alive: HashSet::new(),
                removed: RefCell::new(Vec::new()),
                removable: HashSet::new(),
            }
        }

        fn run<F>(&self, ledger: &mut Ledger, action: F) -> ReapReport
        where
            F: FnOnce(&mut Ledger, &ReapContext<'_>) -> ReapReport,
        {
            let is_alive = |owner: &Owner| self.alive.contains(&owner.pid);
            let observe = |path: &Path| -> SysResult<Observation> {
                let path = path.to_string_lossy().into_owned();
                Ok(self
                    .present
                    .iter()
                    .find(|(candidate, _, _)| *candidate == path)
                    .map(|(_, index, links)| Observation::Present {
                        file_id: FileId {
                            volume_serial: 7,
                            index: *index,
                        },
                        links: *links,
                    })
                    .unwrap_or(Observation::Missing))
            };
            let remove_directory = |path: &Path| -> SysResult<()> {
                let text = path.to_string_lossy().into_owned();
                if self.removable.contains(&text) {
                    self.removed.borrow_mut().push(path.to_path_buf());
                    Ok(())
                } else {
                    Err(SysError::Invalid("the directory is not empty".into()))
                }
            };
            let context = ReapContext {
                writer: &self.writer,
                is_alive: &is_alive,
                observe: &observe,
                remove_directory: &remove_directory,
            };
            action(ledger, &context)
        }
    }

    #[test]
    fn a_dead_sessions_ace_is_revoked_and_its_row_removed() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1", 999));
        let mut harness = Harness::new();
        harness.present.push((r"C:\work\vendor".into(), 10, 1));

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert_eq!(report.revoked, vec![r"C:\work\vendor".to_string()]);
        assert!(ledger.entries.is_empty());
        assert_eq!(
            *harness.writer.revoked.borrow(),
            vec![r"C:\work\vendor".to_string()]
        );
    }

    #[test]
    fn a_live_sessions_ace_is_left_alone() {
        // The failure this guards is the worst one available: stripping the
        // confinement off a command that is running right now.
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1", 42));
        let mut harness = Harness::new();
        harness.alive.insert(42);
        harness.present.push((r"C:\work\vendor".into(), 10, 1));

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert!(report.is_empty());
        assert_eq!(ledger.entries.len(), 1);
        assert!(harness.writer.revoked.borrow().is_empty());
    }

    #[test]
    fn a_replaced_file_is_refused_and_the_row_stays() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1", 999));
        let mut harness = Harness::new();
        // Same path, different file: someone renamed or recreated it.
        harness.present.push((r"C:\work\vendor".into(), 77, 1));

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert!(harness.writer.revoked.borrow().is_empty());
        assert_eq!(report.refused.len(), 1);
        assert!(report.needs_attention());
        assert_eq!(
            ledger.entries.len(),
            1,
            "a refused row must stay; it is the only record that the ACE exists"
        );
    }

    #[test]
    fn a_hard_linked_file_is_refused() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\shared", 10, "s1", 999));
        let mut harness = Harness::new();
        harness.present.push((r"C:\work\shared".into(), 10, 3));

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert_eq!(report.refused.len(), 1);
        assert!(report.refused[0].1.contains("hard links"));
        assert_eq!(ledger.entries.len(), 1);
    }

    #[test]
    fn a_vanished_path_drops_the_row_without_a_disk_write() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\gone", 10, "s1", 999));
        let harness = Harness::new();

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert!(harness.writer.revoked.borrow().is_empty());
        assert_eq!(report.dropped.len(), 1);
        assert!(ledger.entries.is_empty());
    }

    #[test]
    fn a_failed_revoke_keeps_the_row() {
        // The distinction that matters: a failure is recoverable as long as
        // the row survives, and permanent the moment it does not.
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1", 999));
        let mut harness = Harness::new();
        harness.present.push((r"C:\work\vendor".into(), 10, 1));
        harness.writer.fail_on.insert(r"C:\work\vendor".into());

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert_eq!(report.failed.len(), 1);
        assert!(report.needs_attention());
        assert_eq!(ledger.entries.len(), 1);
    }

    #[test]
    fn an_observation_failure_keeps_the_row_too() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1", 999));
        let harness = Harness::new();

        let is_alive = |_: &Owner| false;
        let observe = |_: &Path| -> SysResult<Observation> {
            Err(SysError::Invalid("the volume is offline".into()))
        };
        let remove_directory = |_: &Path| -> SysResult<()> { Ok(()) };
        let context = ReapContext {
            writer: &harness.writer,
            is_alive: &is_alive,
            observe: &observe,
            remove_directory: &remove_directory,
        };

        let report = reap_dead_sessions(&mut ledger, &context);

        assert_eq!(report.failed.len(), 1);
        assert_eq!(ledger.entries.len(), 1);
    }

    #[test]
    fn a_shared_ace_survives_until_the_last_holder_goes() {
        // Two sessions denied the same path. Reaping the first must not
        // unconfine the second, and reaping the second must not leave the
        // ACE behind.
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1", 998));
        ledger.record(entry("b", r"C:\work\vendor", 10, "s2", 42));
        let mut harness = Harness::new();
        harness.alive.insert(42);
        harness.present.push((r"C:\work\vendor".into(), 10, 1));

        let first = harness.run(&mut ledger, reap_dead_sessions);
        assert_eq!(first.dropped.len(), 1);
        assert!(harness.writer.revoked.borrow().is_empty());
        assert_eq!(ledger.entries.len(), 1);

        // Now the second session dies too.
        let mut harness = Harness::new();
        harness.present.push((r"C:\work\vendor".into(), 10, 1));
        let second = harness.run(&mut ledger, reap_dead_sessions);

        assert_eq!(second.revoked.len(), 1);
        assert!(ledger.entries.is_empty());
    }

    #[test]
    fn nested_aces_come_off_innermost_first() {
        // The grant on the root was placed before the deny inside it, so the
        // deny has to come off first or its placeholder chain is orphaned
        // under a directory that has already gone.
        let mut ledger = Ledger::default();
        let mut root = entry("root", r"C:\work", 10, "s1", 999);
        root.kind = AceKind::AllowWrite;
        root.placed_at_ms = 100;
        let mut inner = entry("inner", r"C:\work\vendor", 11, "s1", 999);
        inner.placed_at_ms = 200;
        ledger.record(root);
        ledger.record(inner);

        let mut harness = Harness::new();
        harness.present.push((r"C:\work".into(), 10, 1));
        harness.present.push((r"C:\work\vendor".into(), 11, 1));

        harness.run(&mut ledger, reap_dead_sessions);

        assert_eq!(
            *harness.writer.revoked.borrow(),
            vec![r"C:\work\vendor".to_string(), r"C:\work".to_string()]
        );
    }

    #[test]
    fn placeholder_directories_are_removed_innermost_first() {
        let mut ledger = Ledger::default();
        let mut row = entry("a", r"C:\work\a\b", 10, "s1", 999);
        row.placeholders = vec![r"C:\work\a".into(), r"C:\work\a\b".into()];
        ledger.record(row);

        let mut harness = Harness::new();
        harness.present.push((r"C:\work\a\b".into(), 10, 1));
        harness.removable.insert(r"C:\work\a".into());
        harness.removable.insert(r"C:\work\a\b".into());

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert_eq!(
            report.placeholders_removed,
            vec![PathBuf::from(r"C:\work\a\b"), PathBuf::from(r"C:\work\a")]
        );
    }

    #[test]
    fn a_placeholder_someone_put_files_in_is_left_where_it_is() {
        let mut ledger = Ledger::default();
        let mut row = entry("a", r"C:\work\a", 10, "s1", 999);
        row.placeholders = vec![r"C:\work\a".into()];
        ledger.record(row);

        let mut harness = Harness::new();
        harness.present.push((r"C:\work\a".into(), 10, 1));
        // Not in `removable`: the directory is no longer empty.

        let report = harness.run(&mut ledger, reap_dead_sessions);

        assert_eq!(report.revoked.len(), 1, "the ACE still comes off");
        assert!(report.placeholders_removed.is_empty());
    }

    #[test]
    fn revoking_one_session_leaves_the_others_alone() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\a", 10, "s1", 1));
        ledger.record(entry("b", r"C:\b", 11, "s2", 2));
        let mut harness = Harness::new();
        harness.alive.insert(1);
        harness.alive.insert(2);
        harness.present.push((r"C:\a".into(), 10, 1));
        harness.present.push((r"C:\b".into(), 11, 1));

        let report = harness.run(&mut ledger, |ledger, context| {
            revoke_session(ledger, "s1", context)
        });

        assert_eq!(report.revoked, vec![r"C:\a".to_string()]);
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].session_id, "s2");
    }

    #[test]
    fn an_empty_ledger_reaps_to_nothing() {
        let mut ledger = Ledger::default();
        let harness = Harness::new();
        assert!(harness.run(&mut ledger, reap_dead_sessions).is_empty());
    }

    #[test]
    fn a_clean_reap_needs_no_attention() {
        let mut ledger = Ledger::default();
        ledger.record(entry("a", r"C:\work\vendor", 10, "s1", 999));
        let mut harness = Harness::new();
        harness.present.push((r"C:\work\vendor".into(), 10, 1));

        assert!(!harness
            .run(&mut ledger, reap_dead_sessions)
            .needs_attention());
    }
}
