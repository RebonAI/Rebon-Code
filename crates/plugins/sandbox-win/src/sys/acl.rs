//! Writing and removing ACEs.
//!
//! What is here is the seam: the trait the rest of the helper is written
//! against, the order check that has to run before any DACL is handed to the OS,
//! and [`SystemAceWriter`], which forwards to the real Win32 calls in
//! [`crate::sys::acl_win32`].
//!
//! The seam is not ceremony. It is what lets [`crate::sys::reap`] and
//! [`crate::sys::apply`] — the parts that decide *whether* an ACE may go on or
//! come off, which is the security-critical half — be written and tested in full
//! against a fake writer, on any platform.
//!
//! ## The order check
//!
//! [`check_order`] runs immediately before a DACL is written, every time. It is
//! two lines and it catches the one mistake in this whole area that leaves no
//! trace: Windows evaluates a DACL top down and stops at the first match, so a
//! deny ACE that sorts after a grant is never consulted. Such a DACL applies
//! cleanly, reads back exactly as written, and enforces nothing.

use crate::core::acl::{is_correctly_ordered, AceKind, PlannedAce};
use crate::sys::{SysError, SysResult};
use std::path::Path;

/// Refuse a DACL whose denies do not all precede its grants.
///
/// A panic would be defensible — this can only fail through a programming error
/// — but the helper is the thing standing between a model's command and the
/// user's disk, and a refusal that names the problem is more useful than a stack
/// trace.
pub fn check_order(aces: &[PlannedAce]) -> SysResult<()> {
    if is_correctly_ordered(aces) {
        return Ok(());
    }
    Err(SysError::Invalid(
        "a deny ACE was ordered after a grant; Windows stops at the first match, so the \
         denial would never be reached"
            .into(),
    ))
}

/// Somewhere ACEs can be put and taken away.
///
/// A trait so [`crate::sys::reap`] can be tested without touching a real disk.
pub trait AceWriter {
    /// Place one ACE on `path` for `trustee_sid`.
    fn place(&self, path: &Path, ace: &PlannedAce, trustee_sid: &str) -> SysResult<()>;

    /// Remove the ACE of `kind` for `trustee_sid` from `path`.
    ///
    /// Must be idempotent: `reap` runs at the start of every `exec`, and an ACE a
    /// previous run already removed is a success, not a failure.
    fn revoke(&self, path: &Path, kind: AceKind, trustee_sid: &str) -> SysResult<()>;
}

/// The real thing.
///
/// Needs no elevation: a DACL change takes `WRITE_DAC` on the path, which the
/// calling user has on their own files and nowhere else. See
/// [`crate::sys::acl_win32`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAceWriter;

impl AceWriter for SystemAceWriter {
    fn place(&self, path: &Path, ace: &PlannedAce, trustee_sid: &str) -> SysResult<()> {
        // Checked here as well as on the whole plan: this is the last point before the
        // DACL is handed to the OS, and a wrongly ordered one applies cleanly and
        // enforces nothing.
        check_order(std::slice::from_ref(ace))?;
        crate::sys::acl_win32::place(path, ace, trustee_sid)
    }

    fn revoke(&self, path: &Path, kind: AceKind, trustee_sid: &str) -> SysResult<()> {
        crate::sys::acl_win32::revoke(path, kind, trustee_sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::acl::AceOrigin;
    use std::path::PathBuf;

    fn ace(kind: AceKind) -> PlannedAce {
        PlannedAce {
            path: PathBuf::from(r"C:\work"),
            kind,
            origin: match kind {
                AceKind::DenyRead | AceKind::DenyExecute => AceOrigin::DenyRead,
                AceKind::DenyWrite => AceOrigin::DenyWrite,
                AceKind::AllowWrite => AceOrigin::AllowWrite,
            },
        }
    }

    #[test]
    fn a_correctly_ordered_dacl_passes() {
        assert!(check_order(&[ace(AceKind::DenyWrite), ace(AceKind::AllowWrite)]).is_ok());
        assert!(check_order(&[]).is_ok());
    }

    #[test]
    fn a_grant_ahead_of_a_deny_is_refused_with_the_reason() {
        let error = check_order(&[ace(AceKind::AllowWrite), ace(AceKind::DenyWrite)]).unwrap_err();
        assert!(error.to_string().contains("first match"), "{error}");
    }

    #[test]
    fn the_system_writer_refuses_rather_than_silently_doing_nothing() {
        // A no-op `Ok(())` here would let `exec` report a confined command with no ACE
        // behind any of its rules, so a path that cannot be touched has to come back as
        // an error.
        let writer = SystemAceWriter;
        let absent = Path::new(r"C:\rebon-no-such-path-9f3a\x");
        assert!(writer
            .place(absent, &ace(AceKind::DenyWrite), "S-1-5-32-546")
            .is_err());
        assert!(writer
            .revoke(absent, AceKind::DenyWrite, "S-1-5-32-546")
            .is_err());
    }

    #[test]
    fn the_system_writer_refuses_a_malformed_trustee() {
        // The trustee is a SID the caller looked up; a string that is not one means a
        // bug, and writing a DACL from it would be worse than stopping.
        let writer = SystemAceWriter;
        assert!(writer
            .place(
                Path::new(r"C:\work"),
                &ace(AceKind::AllowWrite),
                "not-a-sid"
            )
            .is_err());
    }
}
