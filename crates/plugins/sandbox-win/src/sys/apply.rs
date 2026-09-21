//! Putting a command's ACEs on the disk.
//!
//! The mirror of [`crate::sys::reap`], and deliberately shaped like it: the
//! decisions are functions over injected closures so they can be tested on any
//! platform against a fake writer, and only the wiring is Windows.
//!
//! ## Recorded before placed, always
//!
//! An ACE is a persistent change to the user's real disk. The ledger is the only
//! thing that knows how to take one back. So the two steps are ordered so that a
//! crash between them errs towards a row with no ACE rather than an ACE with no
//! row:
//!
//! * **Row without an ACE** — the next `reap` calls `revoke`, which is
//!   idempotent, finds nothing, and drops the row. Nobody notices.
//! * **ACE without a row** — nothing on the machine knows the ACE exists. The
//!   user's own editor stops being able to open a file in their own project, and
//!   no amount of reading Rebon's state explains why. Only `uninstall` clears it,
//!   and only because it revokes by ledger — which this ACE is not in. In
//!   practice it is permanent.
//!
//! That is why this module splits into [`prepare`] and [`place`] instead of doing
//! both in one pass: the caller writes the ledger between them.
//!
//! ## Placeholders
//!
//! A rule about a path that does not exist yet still has to bite. The missing
//! chain is created so the name is occupied and can carry the ACE, and what was
//! created is recorded so revocation can take it back out. Without it there is a
//! substitution: delete the denied target, let the sandbox recreate a directory
//! of the same name, and the deny is gone while the rule still reads as though it
//! is in force.

use crate::core::acl::PlannedAce;
use crate::core::ledger::{missing_ancestors, LedgerEntry, Observation, Owner};
use crate::sys::acl::{check_order, AceWriter};
use crate::sys::{SysError, SysResult};
use std::path::{Path, PathBuf};

/// One ACE, ready to be recorded and then written.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub ace: PlannedAce,
    pub entry: LedgerEntry,
}

/// What [`place`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Ledger ids whose ACE is now on the disk.
    pub placed: Vec<String>,
    /// Ledger ids whose ACE could not be written, with why. The caller drops
    /// these rows: a row for an ACE that was never placed would make the next
    /// revocation report a mismatch on a path nothing was ever done to.
    pub failed: Vec<(String, String)>,
}

impl ApplyReport {
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Everything [`prepare`] needs from the outside world.
pub struct PrepareContext<'a> {
    pub exists: &'a dyn Fn(&Path) -> bool,
    pub create_directory: &'a dyn Fn(&Path) -> SysResult<()>,
    pub observe: &'a dyn Fn(&Path) -> SysResult<Observation>,
    /// A fresh, unique ledger id.
    pub new_id: &'a dyn Fn() -> String,
    pub owner: Owner,
    pub trustee_sid: String,
    pub now_ms: u64,
}

/// Create any missing path, read its identity, and build its ledger row.
///
/// Does not touch a DACL. Everything here is either a read or the creation of
/// a directory the rule asked to exist, so a failure leaves the machine's
/// access control exactly as it was.
///
/// Refuses the whole set on the first failure rather than applying part of
/// it. A half-applied rule set is a command running with some of its
/// confinement — which reads as confined to everything downstream and is not.
pub fn prepare(aces: &[PlannedAce], context: &PrepareContext<'_>) -> SysResult<Vec<Prepared>> {
    // Checked once for the whole plan before any of it is acted on. Windows
    // stops at the first matching ACE, so a deny that sorts after a grant is
    // decoration — and a DACL in that state applies cleanly and reads back
    // exactly as written.
    check_order(aces)?;

    let session_id = context.owner.session_id();
    let mut prepared = Vec::with_capacity(aces.len());

    for ace in aces {
        let placeholders = create_placeholders(&ace.path, context)?;

        let observed = (context.observe)(&ace.path)?;
        let (file_id, links) = match observed {
            Observation::Present { file_id, links } => (file_id, links),
            Observation::Missing => {
                return Err(SysError::Invalid(format!(
                    "{} still does not exist after creating the directories leading to it, so \
                     no rule can be attached to it",
                    ace.path.display()
                )))
            }
        };

        // The same refusal `reap` makes on the way out, made here on the way
        // in — an ACE placed through one name of a hard-linked file applies
        // to every other name too, and there is no honest way to take it back
        // through one of them. Better to refuse the rule than to enforce
        // something wider than what was asked for.
        if links > 1 {
            return Err(SysError::Invalid(format!(
                "{} has {links} hard links; a rule placed through one name applies to all of \
                 them and cannot be taken back through one, so it is refused rather than \
                 silently applied to paths that were never named",
                ace.path.display()
            )));
        }

        prepared.push(Prepared {
            ace: ace.clone(),
            entry: LedgerEntry {
                id: (context.new_id)(),
                path: ace.path.to_string_lossy().into_owned(),
                file_id,
                kind: ace.kind,
                trustee_sid: context.trustee_sid.clone(),
                session_id: session_id.clone(),
                owner: context.owner,
                placeholders: placeholders
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
                placed_at_ms: context.now_ms,
            },
        });
    }

    Ok(prepared)
}

/// Write the ACEs. The caller must already have recorded the rows.
///
/// One failure does not abandon the rest: every row is in the ledger by now,
/// so anything that did land can be taken back. The caller decides what to do
/// with a partial result — for `exec` that is to refuse to run the command,
/// because a command that gets some of its rules is not a lesser version of
/// confined.
pub fn place(prepared: &[Prepared], writer: &dyn AceWriter) -> ApplyReport {
    let mut report = ApplyReport::default();
    for item in prepared {
        match writer.place(&item.ace.path, &item.ace, &item.entry.trustee_sid) {
            Ok(()) => report.placed.push(item.entry.id.clone()),
            Err(error) => report.failed.push((
                item.entry.id.clone(),
                format!("{}: {error}", item.entry.path),
            )),
        }
    }
    report
}

/// Create the directories leading to `path`, outermost first.
fn create_placeholders(path: &Path, context: &PrepareContext<'_>) -> SysResult<Vec<PathBuf>> {
    let missing = missing_ancestors(path, |candidate| (context.exists)(candidate));
    for directory in &missing {
        (context.create_directory)(directory)?;
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::acl::{AceKind, AceOrigin};
    use crate::core::ledger::{FileId, Ledger, RecordOutcome};
    use std::cell::RefCell;
    use std::collections::HashSet;

    fn ace(kind: AceKind, path: &str) -> PlannedAce {
        PlannedAce {
            path: PathBuf::from(path),
            kind,
            origin: match kind {
                AceKind::DenyRead | AceKind::DenyExecute => AceOrigin::DenyRead,
                AceKind::DenyWrite => AceOrigin::DenyWrite,
                AceKind::AllowWrite => AceOrigin::AllowWrite,
            },
        }
    }

    /// A world where paths exist, directories can be made, and every file has
    /// a distinct identity.
    struct World {
        present: RefCell<HashSet<PathBuf>>,
        links: u32,
        created: RefCell<Vec<PathBuf>>,
    }

    impl World {
        fn with(paths: &[&str]) -> Self {
            Self {
                present: RefCell::new(paths.iter().map(PathBuf::from).collect()),
                links: 1,
                created: RefCell::new(Vec::new()),
            }
        }
    }

    fn context<'a>(world: &'a World, counter: &'a RefCell<u32>) -> PrepareContext<'a> {
        PrepareContext {
            exists: Box::leak(Box::new(move |path: &Path| {
                world.present.borrow().contains(path)
            })),
            create_directory: Box::leak(Box::new(move |path: &Path| {
                world.present.borrow_mut().insert(path.to_path_buf());
                world.created.borrow_mut().push(path.to_path_buf());
                Ok(())
            })),
            observe: Box::leak(Box::new(move |path: &Path| {
                if !world.present.borrow().contains(path) {
                    return Ok(Observation::Missing);
                }
                Ok(Observation::Present {
                    file_id: FileId {
                        volume_serial: 1,
                        // Stable per path, distinct across paths.
                        index: path.to_string_lossy().len() as u64 * 1000
                            + path.to_string_lossy().bytes().map(u64::from).sum::<u64>(),
                    },
                    links: world.links,
                })
            })),
            new_id: Box::leak(Box::new(move || {
                let mut next = counter.borrow_mut();
                *next += 1;
                format!("id-{next}")
            })),
            owner: Owner {
                pid: 4321,
                started_at_ms: 1_700_000_000_000,
            },
            trustee_sid: "S-1-5-21-1-2-3-1001".into(),
            now_ms: 1_700_000_001_000,
        }
    }

    struct RecordingWriter {
        placed: RefCell<Vec<(PathBuf, AceKind)>>,
        refuse: Option<PathBuf>,
    }

    impl RecordingWriter {
        fn new() -> Self {
            Self {
                placed: RefCell::new(Vec::new()),
                refuse: None,
            }
        }
    }

    impl AceWriter for RecordingWriter {
        fn place(&self, path: &Path, ace: &PlannedAce, _trustee: &str) -> SysResult<()> {
            if self.refuse.as_deref() == Some(path) {
                return Err(SysError::Invalid("refused by the test".into()));
            }
            self.placed
                .borrow_mut()
                .push((path.to_path_buf(), ace.kind));
            Ok(())
        }

        fn revoke(&self, _path: &Path, _kind: AceKind, _trustee: &str) -> SysResult<()> {
            Ok(())
        }
    }

    #[test]
    fn a_rule_on_an_existing_path_needs_no_placeholder() {
        let world = World::with(&[r"C:\work", r"C:\work\vendor"]);
        let counter = RefCell::new(0);
        let prepared = prepare(
            &[ace(AceKind::DenyWrite, r"C:\work\vendor")],
            &context(&world, &counter),
        )
        .unwrap();

        assert_eq!(prepared.len(), 1);
        assert!(prepared[0].entry.placeholders.is_empty());
        assert!(world.created.borrow().is_empty());
    }

    /// A root the running platform's own path parser will actually split.
    ///
    /// The `C:\…` literals elsewhere in this module are opaque keys and work
    /// anywhere. The placeholder chain is not: it is walked with
    /// `Path::parent()`, which splits only on the separator of the platform
    /// the test runs on, so a Windows literal is one indivisible component on
    /// the macOS runner CI uses. Production only ever sees Windows paths; the
    /// chain-building being tested here is the same either way.
    fn native_root() -> &'static str {
        if cfg!(windows) {
            r"C:\work"
        } else {
            "/work"
        }
    }

    /// `native_root()` with `descendants` appended, as the platform spells it.
    fn under_root(descendants: &[&str]) -> String {
        let mut path = PathBuf::from(native_root());
        path.extend(descendants);
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_rule_on_a_missing_path_creates_and_records_the_whole_chain() {
        // The substitution this closes: without the placeholder there is no
        // object to attach the deny to, and the sandbox creates the path
        // itself — unencumbered.
        let world = World::with(&[native_root()]);
        let counter = RefCell::new(0);
        let prepared = prepare(
            &[ace(AceKind::DenyRead, &under_root(&["a", "b", "secrets"]))],
            &context(&world, &counter),
        )
        .unwrap();

        assert_eq!(
            prepared[0].entry.placeholders,
            vec![
                under_root(&["a"]),
                under_root(&["a", "b"]),
                under_root(&["a", "b", "secrets"]),
            ]
        );
        // Outermost first, so removal can run back to front.
        assert_eq!(
            *world.created.borrow(),
            vec![
                PathBuf::from(under_root(&["a"])),
                PathBuf::from(under_root(&["a", "b"])),
                PathBuf::from(under_root(&["a", "b", "secrets"])),
            ]
        );
    }

    #[test]
    fn a_hard_linked_target_is_refused_rather_than_confined() {
        // The ACE would apply through every other name of the same file, and
        // revoking through this one is ambiguous. Refusing is the honest
        // answer; the alternative is enforcing a rule on paths nobody named.
        let mut world = World::with(&[r"C:\work\shared.txt"]);
        world.links = 3;
        let counter = RefCell::new(0);

        let error = prepare(
            &[ace(AceKind::DenyRead, r"C:\work\shared.txt")],
            &context(&world, &counter),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("hard link"), "{error}");
        assert!(error.contains("shared.txt"), "{error}");
    }

    #[test]
    fn a_badly_ordered_plan_is_refused_before_anything_is_created() {
        // A deny after a grant applies cleanly and enforces nothing. Caught
        // before the first directory is made, so a refusal leaves no trace.
        let world = World::with(&[r"C:\work"]);
        let counter = RefCell::new(0);

        let error = prepare(
            &[
                ace(AceKind::AllowWrite, r"C:\work\out"),
                ace(AceKind::DenyWrite, r"C:\work\out\vendor"),
            ],
            &context(&world, &counter),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("deny"), "{error}");
        assert!(world.created.borrow().is_empty(), "it created something");
    }

    #[test]
    fn every_row_gets_its_own_id() {
        let world = World::with(&[r"C:\work", r"C:\a", r"C:\b"]);
        let counter = RefCell::new(0);
        let prepared = prepare(
            &[
                ace(AceKind::DenyRead, r"C:\a"),
                ace(AceKind::DenyRead, r"C:\b"),
            ],
            &context(&world, &counter),
        )
        .unwrap();

        assert_ne!(prepared[0].entry.id, prepared[1].entry.id);
    }

    #[test]
    fn the_rows_all_belong_to_the_callers_session() {
        // Not the helper's own: the helper exits with the command, and rows
        // owned by it would be reapable before the next command started.
        let world = World::with(&[r"C:\work", r"C:\a"]);
        let counter = RefCell::new(0);
        let context = context(&world, &counter);
        let expected = context.owner.session_id();

        let prepared = prepare(&[ace(AceKind::DenyRead, r"C:\a")], &context).unwrap();

        assert_eq!(prepared[0].entry.session_id, expected);
        assert_eq!(prepared[0].entry.owner.pid, 4321);
    }

    #[test]
    fn placing_reports_each_row_that_landed() {
        let world = World::with(&[r"C:\work", r"C:\a", r"C:\b"]);
        let counter = RefCell::new(0);
        let prepared = prepare(
            &[
                ace(AceKind::DenyRead, r"C:\a"),
                ace(AceKind::DenyRead, r"C:\b"),
            ],
            &context(&world, &counter),
        )
        .unwrap();

        let writer = RecordingWriter::new();
        let report = place(&prepared, &writer);

        assert!(report.is_ok());
        assert_eq!(report.placed.len(), 2);
        assert_eq!(writer.placed.borrow().len(), 2);
    }

    #[test]
    fn a_write_that_fails_is_named_and_the_rest_still_go_on() {
        // Every row is already in the ledger by this point, so continuing is
        // safe — and stopping would leave the caller unable to say which
        // rules are in force.
        let world = World::with(&[r"C:\work", r"C:\a", r"C:\b"]);
        let counter = RefCell::new(0);
        let prepared = prepare(
            &[
                ace(AceKind::DenyRead, r"C:\a"),
                ace(AceKind::DenyRead, r"C:\b"),
            ],
            &context(&world, &counter),
        )
        .unwrap();

        let mut writer = RecordingWriter::new();
        writer.refuse = Some(PathBuf::from(r"C:\a"));
        let report = place(&prepared, &writer);

        assert!(!report.is_ok());
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].1.contains(r"C:\a"), "{:?}", report.failed);
        assert_eq!(report.placed.len(), 1);
    }

    #[test]
    fn the_same_rule_twice_in_one_session_is_recorded_once() {
        // Re-entry after a crash lands here. Writing the ACE again is
        // harmless; recording it twice is not — the second revocation would
        // find nothing and report a mismatch on a path that is fine.
        let world = World::with(&[r"C:\work", r"C:\a"]);
        let counter = RefCell::new(0);
        let context = context(&world, &counter);
        let first = prepare(&[ace(AceKind::DenyRead, r"C:\a")], &context).unwrap();
        let second = prepare(&[ace(AceKind::DenyRead, r"C:\a")], &context).unwrap();

        let mut ledger = Ledger::default();
        assert_eq!(ledger.record(first[0].entry.clone()), RecordOutcome::Added);
        assert_eq!(
            ledger.record(second[0].entry.clone()),
            RecordOutcome::AlreadyRecorded
        );
        assert_eq!(ledger.entries.len(), 1);
    }
}
