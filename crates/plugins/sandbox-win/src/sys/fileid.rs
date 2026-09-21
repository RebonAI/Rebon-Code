//! File identity.
//!
//! Revocation compares the file it is about to strip an ACE from against the one
//! the ledger recorded, using volume serial + file index rather than the path. A
//! path is a name, and names get reused: delete `C:\work\vendor`, create a new
//! directory with the same name, and revoking by path takes the ACE off something
//! the helper never touched. The index is the file.
//!
//! The hard-link count comes back with it because an ACE reached through one name
//! applies to every name the file has, so revoking "this one" has no single
//! meaning — the ledger refuses rather than guessing.

use crate::core::ledger::Observation;
use crate::sys::SysResult;
use std::path::Path;

/// What the disk says about `path` right now.
///
/// A path that does not resolve is [`Observation::Missing`], not an error: the
/// file is gone and so is its security descriptor, which is a normal thing to
/// find at revocation time.
pub fn observe(path: &Path) -> SysResult<Observation> {
    imp::observe(path)
}

#[cfg(windows)]
mod imp {
    use crate::core::ledger::{FileId, Observation};
    use crate::sys::{SysError, SysResult};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    pub fn observe(path: &Path) -> SysResult<Observation> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

        // `FILE_READ_ATTRIBUTES` only — the helper is identifying the file, not reading
        // it, and a deny-read ACE it placed itself would refuse anything more.
        // `BACKUP_SEMANTICS` is what makes a directory openable at all.
        // `OPEN_REPARSE_POINT` keeps a symlink from redirecting the identity check to
        // its target: following it is how a revoke ends up on a file the ledger never
        // named.
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
                return Ok(Observation::Missing);
            }
            return Err(SysError::win32("CreateFileW", code));
        }

        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(handle, &mut information) };
        let code = unsafe { GetLastError() };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(SysError::win32("GetFileInformationByHandle", code));
        }

        Ok(Observation::Present {
            file_id: FileId {
                volume_serial: information.dwVolumeSerialNumber,
                index: ((information.nFileIndexHigh as u64) << 32)
                    | information.nFileIndexLow as u64,
            },
            links: information.nNumberOfLinks,
        })
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::core::ledger::Observation;
    use crate::sys::{SysError, SysResult};
    use std::path::Path;

    pub fn observe(_path: &Path) -> SysResult<Observation> {
        Err(SysError::Unsupported("file identity"))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::core::ledger::Observation;

    #[test]
    fn a_missing_path_is_missing_rather_than_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let absent = temp.path().join("never-created");

        assert_eq!(observe(&absent).unwrap(), Observation::Missing);
    }

    #[test]
    fn a_file_reports_one_link_and_a_stable_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        std::fs::write(&path, b"x").unwrap();

        let first = observe(&path).unwrap();
        let second = observe(&path).unwrap();

        assert_eq!(first, second, "identity must not change between reads");
        match first {
            Observation::Present { links, .. } => assert_eq!(links, 1),
            other => panic!("expected a present file, got {other:?}"),
        }
    }

    #[test]
    fn a_directory_can_be_identified_too() {
        // Needs FILE_FLAG_BACKUP_SEMANTICS; without it this is ERROR_ACCESS_DENIED and
        // every `--allow-write` root would fail to record.
        let temp = tempfile::tempdir().unwrap();
        assert!(matches!(
            observe(temp.path()).unwrap(),
            Observation::Present { .. }
        ));
    }

    #[test]
    fn two_different_files_have_different_identities() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("a");
        let second = temp.path().join("b");
        std::fs::write(&first, b"a").unwrap();
        std::fs::write(&second, b"b").unwrap();

        assert_ne!(observe(&first).unwrap(), observe(&second).unwrap());
    }

    #[test]
    fn a_recreated_path_is_a_different_file() {
        // The exact substitution the ledger's identity check exists to catch: delete the
        // denied target, recreate the name, and a path-keyed revoke would strip the ACE
        // from the impostor.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("vendor");
        std::fs::write(&path, b"first").unwrap();
        let before = observe(&path).unwrap();

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"second").unwrap();
        let after = observe(&path).unwrap();

        assert_ne!(
            before, after,
            "a recreated path must not look like the same file"
        );
    }

    #[test]
    fn a_hard_link_is_visible_as_a_link_count() {
        // Needs NTFS; a filesystem without hard links skips rather than failing, because
        // the assertion is about what we report when the OS gives us the count, not about
        // the OS.
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original.txt");
        let link = temp.path().join("link.txt");
        std::fs::write(&original, b"x").unwrap();
        if std::fs::hard_link(&original, &link).is_err() {
            return;
        }

        match observe(&original).unwrap() {
            Observation::Present { links, .. } => assert!(
                links > 1,
                "a hard-linked file must report more than one link, or revocation \
                 will silently affect names the ledger never recorded"
            ),
            other => panic!("expected a present file, got {other:?}"),
        }
    }

    #[test]
    fn both_names_of_a_hard_linked_file_share_one_identity() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original.txt");
        let link = temp.path().join("link.txt");
        std::fs::write(&original, b"x").unwrap();
        if std::fs::hard_link(&original, &link).is_err() {
            return;
        }

        match (observe(&original).unwrap(), observe(&link).unwrap()) {
            (Observation::Present { file_id: a, .. }, Observation::Present { file_id: b, .. }) => {
                assert_eq!(a, b, "this is why revoking through one name is ambiguous")
            }
            other => panic!("expected two present files, got {other:?}"),
        }
    }
}
