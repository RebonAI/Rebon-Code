//! The Win32 half of `sandbox-win.exe`.
//!
//! Everything here is a syscall wrapped in a value. [`crate::core`] decides
//! *what* should be true; this module finds out what *is* true and makes it so.
//! The split is deliberate and load-bearing: the judgement lives where it can be
//! tested on any machine, and this layer stays thin enough to review by eye.
//!
//! Off Windows every entry point compiles to [`SysError::Unsupported`], so a
//! workspace check works on a mac without a Windows crate breaking the build.
//!
//! ## What each module does
//!
//! * [`principal`] — SID lookup for the sandbox accounts and the caller.
//! * [`provisioning`] — creating and removing the accounts, the shared group,
//!   their logon rights, the sign-in-screen hiding, and their profiles.
//! * [`credentials`] — DPAPI store and load, and the "two credentials decrypt"
//!   test behind `credentials=ok`.
//! * [`fileid`] — the volume-serial + file-index identity revocation compares
//!   against, and the hard-link count that makes it refuse.
//! * [`process`] — PID + creation-time liveness, so `reap` cannot mistake a
//!   recycled PID for a live session.
//! * [`ledger_store`] — the ledger on disk: locked, written through a temp file,
//!   renamed over.
//! * [`acl_win32`] / [`apply`] — writing and removing the deny/allow ACEs, and
//!   the placeholder chain a rule on a not-yet-existing path needs.
//! * [`wfp`] / [`wfp_install`] — probing and installing the network filters.
//! * [`launch`] — starting the confined process as the downgraded account and
//!   relaying its output and exit code.
//! * [`cmdline`] — the real `CommandLineToArgvW`, so the core's re-quoting can be
//!   checked against the function that will actually read it.

pub mod acl;
pub mod acl_win32;
pub mod apply;
pub mod cmdline;
pub mod credentials;
pub mod desktop;
pub mod elevation;
pub mod fileid;
pub mod launch;
pub mod ledger_store;
pub mod paths;
pub mod principal;
pub mod process;
pub mod provisioning;
pub mod random;
pub mod reap;
pub mod wfp;
pub mod wfp_install;

/// Why a system call could not answer.
#[derive(Debug, thiserror::Error)]
pub enum SysError {
    /// The build is not for Windows. Not an error condition so much as a statement
    /// that this crate has nothing to say here.
    #[error("{0} is only available on Windows")]
    Unsupported(&'static str),

    /// A Win32 call failed. `code` is `GetLastError`, or the HRESULT-ish status the
    /// API returned in its own convention.
    #[error("{operation} failed: {detail} (code {code})")]
    Win32 {
        operation: &'static str,
        detail: String,
        code: u32,
    },

    #[error("{operation} failed: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{0}")]
    Invalid(String),
}

impl SysError {
    pub fn win32(operation: &'static str, code: u32) -> Self {
        SysError::Win32 {
            operation,
            detail: describe_win32(code),
            code,
        }
    }

    pub fn io(operation: &'static str, source: std::io::Error) -> Self {
        SysError::Io { operation, source }
    }

    /// Whether this is "you are not allowed to look", as opposed to "the thing is
    /// not there".
    ///
    /// The distinction decides what `status` may claim: a probe that could not read
    /// must report the piece as missing, never as present, and must not be confused
    /// with a probe that read successfully and found nothing.
    pub fn is_access_denied(&self) -> bool {
        matches!(
            self,
            SysError::Win32 {
                code: ERROR_ACCESS_DENIED,
                ..
            }
        ) || matches!(
            self,
            SysError::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied
        )
    }
}

const ERROR_ACCESS_DENIED: u32 = 5;

fn describe_win32(code: u32) -> String {
    // Only the codes whose meaning changes what a reader should do. A full
    // `FormatMessageW` table would be longer than the module and mostly repeat the
    // number back.
    match code {
        2 => "not found".into(),
        3 => "path not found".into(),
        5 => "access denied".into(),
        6 => "invalid handle".into(),
        32 => "the file is in use by another process".into(),
        87 => "invalid parameter".into(),
        1332 => "no such account".into(),
        1376 => "no such local group".into(),
        // The FWP_E_* range. Spelled out because they arrive as ten-digit decimals in
        // `GetLastError` style output, where 2150760453 tells a reader nothing at all.
        0x8032_0005 => "the WFP provider is not installed".into(),
        0x8032_0006 => "the WFP provider context is not installed".into(),
        0x8032_0007 => "the WFP sublayer is not installed".into(),
        0x8032_0015 => "the filter engine is not running".into(),
        other => format!("Win32 error {other} (0x{other:08x})"),
    }
}

pub type SysResult<T> = Result<T, SysError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_denied_is_distinguishable_from_absence() {
        // `status` reports a piece as missing either way, but only one of these means
        // "reinstall"; the other means "we could not look".
        assert!(SysError::win32("FwpmEngineOpen0", ERROR_ACCESS_DENIED).is_access_denied());
        assert!(!SysError::win32("LookupAccountNameW", 1332).is_access_denied());
    }

    #[test]
    fn a_win32_error_names_the_call_and_the_code() {
        let text = SysError::win32("LookupAccountNameW", 1332).to_string();
        assert!(text.contains("LookupAccountNameW"), "{text}");
        assert!(text.contains("no such account"), "{text}");
        assert!(text.contains("1332"), "{text}");
    }

    #[test]
    fn an_unknown_code_still_reports_the_number() {
        assert!(SysError::win32("X", 4242).to_string().contains("4242"));
    }
}
