//! Where the helper keeps its own state.
//!
//! `%LOCALAPPDATA%\Rebon\sandbox-win\`, holding two files:
//!
//! * `credentials.bin` — DPAPI-protected, user scope, and with a DACL that
//!   explicitly denies the sandbox group. That deny is not ceremony: the file
//!   holds both accounts' passwords, and a confined command that reads it can
//!   log on as the account with no network filters.
//! * `ledger.json` — every ACE placed, so a killed session's changes to the
//!   user's real disk can be undone.
//!
//! `%LOCALAPPDATA%` and not `%APPDATA%`: this is machine-local state that must
//! not roam to another machine, where the SIDs it names mean nothing.
//!
//! No environment override. The ledger path is not a preference — pointing it
//! elsewhere would make `reap` unable to find the ACEs on the disk, which leaves
//! them there permanently with nothing that knows about them.

use crate::sys::{SysError, SysResult};
use std::path::PathBuf;

/// The vendor directory both Rebon and the helper share.
pub const VENDOR_DIRECTORY: &str = "Rebon";
/// The helper's own subdirectory under it.
pub const HELPER_DIRECTORY: &str = "sandbox-win";

pub const CREDENTIALS_FILE_NAME: &str = "credentials.bin";

/// `%LOCALAPPDATA%\Rebon\sandbox-win`.
pub fn data_directory() -> SysResult<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        SysError::Invalid(
            "LOCALAPPDATA is not set, so there is nowhere to keep the sandbox helper's state"
                .into(),
        )
    })?;
    Ok(PathBuf::from(local)
        .join(VENDOR_DIRECTORY)
        .join(HELPER_DIRECTORY))
}

pub fn credentials_path() -> SysResult<PathBuf> {
    Ok(data_directory()?.join(CREDENTIALS_FILE_NAME))
}

pub fn ledger_path() -> SysResult<PathBuf> {
    Ok(data_directory()?.join(crate::core::ledger::LEDGER_FILE_NAME))
}

/// Create the directory if it is not there.
pub fn ensure_data_directory() -> SysResult<PathBuf> {
    let directory = data_directory()?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| SysError::io("creating the sandbox helper's data directory", error))?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_files_live_side_by_side_under_the_helpers_directory() {
        // Only meaningful where LOCALAPPDATA exists; elsewhere the error is the
        // assertion.
        match (credentials_path(), ledger_path()) {
            (Ok(credentials), Ok(ledger)) => {
                assert_eq!(credentials.parent(), ledger.parent());
                assert!(credentials.ends_with("credentials.bin"));
                assert!(ledger.ends_with("ledger.json"));
                assert!(credentials.to_string_lossy().contains(VENDOR_DIRECTORY));
                assert!(credentials.to_string_lossy().contains(HELPER_DIRECTORY));
            }
            (Err(error), _) => assert!(error.to_string().contains("LOCALAPPDATA")),
            (_, Err(error)) => assert!(error.to_string().contains("LOCALAPPDATA")),
        }
    }

    #[test]
    fn the_helper_directory_is_not_the_vendor_directory() {
        // Rebon's own config lives in the vendor directory. The helper's state is
        // security-sensitive and gets its own DACL, so it needs its own directory to put
        // one on.
        assert_ne!(VENDOR_DIRECTORY, HELPER_DIRECTORY);
    }
}
