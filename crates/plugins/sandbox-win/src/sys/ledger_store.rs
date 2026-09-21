//! The ACE ledger on disk.
//!
//! Three properties, and each one exists because of a specific way the ledger
//! can lose an ACE that is sitting on the user's real disk:
//!
//! 1. **A torn file loses everything.** The write goes to a temp file in the
//!    same directory, is flushed to the OS, and is then renamed over the real
//!    one. A reader sees the old file or the new one.
//! 2. **A concurrent write loses one update.** Several sandboxed commands can
//!    run at once, and each `exec` does a read-modify-write. Two processes that
//!    both read `{A}` and write `{A,B}` and `{A,C}` leave one ACE on the disk
//!    with no record of it — atomic rename does not help, because both writes
//!    were individually atomic. So the whole read-modify-write is taken under an
//!    advisory lock on a sibling file.
//! 3. **An unreadable ledger must not read as an empty one.** Treating a corrupt
//!    file as "no ACEs" would silently abandon every ACE recorded in it, which
//!    is the exact outcome the ledger exists to prevent.
//!
//! The lock is a separate file rather than the ledger itself: locking the ledger
//! would mean holding a handle to the file we are about to replace by rename,
//! and on Windows that is how you get a sharing violation instead of a write.

use crate::core::ledger::Ledger;
use crate::sys::{SysError, SysResult};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A ledger file and its lock.
#[derive(Debug, Clone)]
pub struct LedgerStore {
    path: PathBuf,
}

impl LedgerStore {
    /// A store at an explicit path, used by tests and by an explicit `--ledger`
    /// argument; the product path comes from [`LedgerStore::default_location`].
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `%LOCALAPPDATA%\Rebon\sandbox-win\ledger.json`.
    pub fn default_location() -> SysResult<Self> {
        Ok(Self::at(crate::sys::paths::ledger_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        let mut lock = self.path.clone().into_os_string();
        lock.push(".lock");
        PathBuf::from(lock)
    }

    fn temp_path(&self) -> PathBuf {
        let mut temp = self.path.clone().into_os_string();
        temp.push(".tmp");
        PathBuf::from(temp)
    }

    fn directory(&self) -> SysResult<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| SysError::io("creating the ledger directory", error))?;
            }
        }
        Ok(())
    }

    /// Read the ledger without taking the lock.
    ///
    /// Safe for a read-only view (a dry run, a person looking) because the rename
    /// makes every visible state a whole one. Anything that writes must go through
    /// [`LedgerStore::update`] instead.
    pub fn load(&self) -> SysResult<Ledger> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ledger::load(&bytes)
                .map_err(|error| SysError::Invalid(format!("{}: {error}", self.path.display()))),
            // A ledger that was never written is a ledger with no ACEs in it, which is the
            // correct answer and not a failure.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(error) => Err(SysError::io("reading the ledger", error)),
        }
    }

    /// Read, modify, and write the ledger, holding the lock throughout.
    ///
    /// `change` may return a value; it is passed back. Returning `Err` from `change`
    /// leaves the file untouched.
    pub fn update<T, F>(&self, change: F) -> SysResult<T>
    where
        F: FnOnce(&mut Ledger) -> SysResult<T>,
    {
        self.directory()?;
        let guard = self.lock()?;

        let mut ledger = self.load()?;
        let outcome = change(&mut ledger)?;
        self.write(&ledger)?;

        drop(guard);
        Ok(outcome)
    }

    /// Take the exclusive lock and read, without writing.
    ///
    /// For a caller that has to see a stable ledger across several steps — `reap`
    /// decides what to revoke, performs the revocations, and only then records the
    /// result.
    pub fn lock(&self) -> SysResult<LedgerGuard> {
        self.directory()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())
            .map_err(|error| SysError::io("opening the ledger lock", error))?;
        file.lock_exclusive()
            .map_err(|error| SysError::io("locking the ledger", error))?;
        Ok(LedgerGuard { file })
    }

    /// Replace the ledger atomically. The caller must hold the lock.
    pub fn write(&self, ledger: &Ledger) -> SysResult<()> {
        self.directory()?;
        let temp = self.temp_path();
        {
            let mut file = File::create(&temp)
                .map_err(|error| SysError::io("creating the ledger temp file", error))?;
            file.write_all(&ledger.to_bytes())
                .map_err(|error| SysError::io("writing the ledger temp file", error))?;
            // Flushed, not fsynced. A crash between here and the rename leaves the previous
            // ledger intact, which is the state `reap` already knows how to recover from;
            // paying an fsync on every ACE would be a real cost for a marginal window.
            file.flush()
                .map_err(|error| SysError::io("flushing the ledger temp file", error))?;
        }
        // `std::fs::rename` uses `MOVEFILE_REPLACE_EXISTING` on Windows, so this
        // overwrites rather than failing on an existing destination.
        std::fs::rename(&temp, &self.path)
            .map_err(|error| SysError::io("replacing the ledger", error))
    }
}

/// Holds the ledger lock for as long as it lives.
#[derive(Debug)]
pub struct LedgerGuard {
    file: File,
}

impl Drop for LedgerGuard {
    fn drop(&mut self) {
        // Best effort: the lock is also released when the handle closes, and the OS
        // releases it if the process dies. A failure here has nothing useful to report
        // to.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::acl::AceKind;
    use crate::core::ledger::{FileId, LedgerEntry, Owner};

    fn entry(id: &str, index: u64) -> LedgerEntry {
        LedgerEntry {
            id: id.to_string(),
            path: format!(r"C:\work\{id}"),
            file_id: FileId {
                volume_serial: 7,
                index,
            },
            kind: AceKind::DenyWrite,
            trustee_sid: "S-1-5-21-1-2-3-1004".into(),
            session_id: "session".into(),
            owner: Owner {
                pid: 1,
                started_at_ms: 2,
            },
            placeholders: Vec::new(),
            placed_at_ms: 3,
        }
    }

    fn store() -> (tempfile::TempDir, LedgerStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = LedgerStore::at(directory.path().join("ledger.json"));
        (directory, store)
    }

    #[test]
    fn a_missing_ledger_reads_as_no_aces() {
        let (_directory, store) = store();
        assert!(store.load().unwrap().entries.is_empty());
    }

    #[test]
    fn an_update_round_trips() {
        let (_directory, store) = store();

        store
            .update(|ledger| {
                ledger.record(entry("a", 10));
                Ok(())
            })
            .unwrap();

        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].id, "a");
    }

    #[test]
    fn updates_accumulate_across_calls() {
        let (_directory, store) = store();
        for id in ["a", "b", "c"] {
            store
                .update(|ledger| {
                    ledger.record(entry(id, id.as_bytes()[0] as u64));
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(store.load().unwrap().entries.len(), 3);
    }

    #[test]
    fn a_failed_change_leaves_the_file_untouched() {
        // The ACE write failed, so the row must not be recorded — a row with no ACE
        // behind it makes the next revoke report a mismatch on a file nobody touched.
        let (_directory, store) = store();
        store
            .update(|ledger| {
                ledger.record(entry("a", 10));
                Ok(())
            })
            .unwrap();

        let result: SysResult<()> = store.update(|ledger| {
            ledger.record(entry("b", 11));
            Err(SysError::Invalid("the ACE could not be written".into()))
        });

        assert!(result.is_err());
        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].id, "a");
    }

    #[test]
    fn a_corrupt_ledger_is_an_error_not_an_empty_one() {
        let (directory, store) = store();
        std::fs::write(directory.path().join("ledger.json"), b"{ not json").unwrap();

        let error = store.load().unwrap_err();

        assert!(error.to_string().contains("ledger.json"), "{error}");
    }

    #[test]
    fn the_temp_file_does_not_survive_a_successful_write() {
        let (directory, store) = store();
        store.update(|_| Ok(())).unwrap();
        assert!(!directory.path().join("ledger.json.tmp").exists());
    }

    #[test]
    fn a_replacement_overwrites_rather_than_failing_on_an_existing_file() {
        // The Windows-specific half of "atomic rename": POSIX `rename` replaces, and
        // `MoveFileExW` only does with REPLACE_EXISTING. If this ever regressed, every
        // write after the first would fail.
        let (_directory, store) = store();
        store
            .update(|ledger| {
                ledger.record(entry("a", 10));
                Ok(())
            })
            .unwrap();
        store
            .update(|ledger| {
                ledger.remove("a");
                ledger.record(entry("b", 11));
                Ok(())
            })
            .unwrap();

        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].id, "b");
    }

    #[test]
    fn the_lock_serialises_concurrent_updates() {
        // The regression this guards is a lost ACE: without the lock two
        // `exec`s that both read `{A}` write `{A,B}` and `{A,C}`, and one of
        // those ACEs ends up on the disk with nothing recording it.
        let (directory, _store) = store();
        let path = directory.path().join("ledger.json");

        let handles: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let store = LedgerStore::at(path);
                    store
                        .update(|ledger| {
                            ledger.record(entry(&format!("entry-{index}"), index as u64));
                            Ok(())
                        })
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let reloaded = LedgerStore::at(&path).load().unwrap();
        assert_eq!(
            reloaded.entries.len(),
            8,
            "an update was lost: {:?}",
            reloaded.entries.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_held_lock_is_released_when_the_guard_drops() {
        let (_directory, store) = store();
        {
            let _guard = store.lock().unwrap();
        }
        // Would block forever if the first lock had leaked.
        let _guard = store.lock().unwrap();
    }

    #[test]
    fn the_lock_file_is_beside_the_ledger_not_the_ledger_itself() {
        // Locking the ledger would mean holding a handle to the file the
        // write is about to rename over, which on Windows is a sharing
        // violation rather than a write.
        let (directory, store) = store();
        let _guard = store.lock().unwrap();
        assert!(directory.path().join("ledger.json.lock").exists());
        assert!(!directory.path().join("ledger.json").exists());
    }

    #[test]
    fn a_store_can_be_created_in_a_directory_that_does_not_exist_yet() {
        let directory = tempfile::tempdir().unwrap();
        let store = LedgerStore::at(directory.path().join("a").join("b").join("ledger.json"));

        store
            .update(|ledger| {
                ledger.record(entry("a", 10));
                Ok(())
            })
            .unwrap();

        assert_eq!(store.load().unwrap().entries.len(), 1);
    }
}
