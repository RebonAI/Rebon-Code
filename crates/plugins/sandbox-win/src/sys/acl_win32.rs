//! Writing and removing ACEs for real.
//!
//! ## Why this needs no elevation
//!
//! Changing a path's DACL needs `WRITE_DAC` on it, which the calling user has on
//! their own files and nowhere else. That is the hard property the non-elevated
//! design buys — a confined command that somehow reached `sandbox-win exec` still
//! cannot grant itself anything the caller did not already have. It is also what
//! makes this layer testable without an administrator: a temp directory the test
//! owns is enough.
//!
//! ## Why the DACL is built rather than merged
//!
//! The obvious implementation is `SetEntriesInAclW`, which merges one entry into
//! an existing ACL. Two measured facts ruled it out:
//!
//! * **`DENY_ACCESS` replaces rather than adds.** A second deny for the same
//!   trustee overwrites the first, and [`crate::core::acl::plan_aces`] emits
//!   deny-read *and* deny-write on the same path for the same trustee. One of the
//!   two rules would have been dropped, silently, on a DACL that still read back
//!   as correct.
//! * **It does not promise canonical order.** Windows evaluates a DACL top down
//!   and stops at the first match, so an allow ahead of a deny makes the deny
//!   unreachable — again with nothing to see in the DACL itself.
//!
//! So the ACL is assembled here: read what is there, drop the rule being
//! replaced, append, order (explicit denies, explicit allows, inherited), write.
//! [`super::acl::check_order`] checks the plan before any of this is reached, and
//! [`super::acl::SystemAceWriter::place`] checks again at the last point before
//! the OS sees it — two checks, because this is the failure that leaves no trace.

use crate::core::acl::{AceKind, PlannedAce};
use crate::sys::SysResult;
use std::path::Path;

/// Place one ACE on `path` for `trustee_sid`.
pub fn place(path: &Path, ace: &PlannedAce, trustee_sid: &str) -> SysResult<()> {
    imp::place(path, ace, trustee_sid)
}

/// Remove every ACE of `kind` for `trustee_sid` from `path`.
///
/// Idempotent: `reap` runs at the start of every `exec`, and an ACE a
/// previous run already removed is a success.
pub fn revoke(path: &Path, kind: AceKind, trustee_sid: &str) -> SysResult<()> {
    imp::revoke(path, kind, trustee_sid)
}

/// The explicit ACEs currently on `path`, in DACL order.
///
/// For `selftest` and for the tests here: the only way to check that an ACE
/// landed where it was supposed to is to read the DACL back.
pub fn read_aces(path: &Path) -> SysResult<Vec<AceEntry>> {
    imp::read_aces(path)
}

/// One ACE as read back from a DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AceEntry {
    pub sid: String,
    pub mask: u32,
    pub is_deny: bool,
    pub inherited: bool,
    pub flags: u32,
}

#[cfg(windows)]
mod imp {
    use super::AceEntry;
    use crate::core::acl::{AceKind, PlannedAce};
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, PSID};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSidToSidW, GetNamedSecurityInfoW,
        SetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        AddAccessAllowedAceEx, AddAccessDeniedAceEx, GetAce, InitializeAcl, ACCESS_ALLOWED_ACE,
        ACCESS_DENIED_ACE, ACE_HEADER, ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, INHERITED_ACE,
        PSECURITY_DESCRIPTOR,
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;

    fn wide(value: &Path) -> Vec<u16> {
        value.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn wide_str(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// An owned SID parsed from its string form.
    struct OwnedSid(PSID);

    impl OwnedSid {
        fn parse(text: &str) -> SysResult<Self> {
            let mut sid: PSID = null_mut();
            if unsafe { ConvertStringSidToSidW(wide_str(text).as_ptr(), &mut sid) } == 0 {
                return Err(SysError::win32("ConvertStringSidToSidW", unsafe {
                    windows_sys::Win32::Foundation::GetLastError()
                }));
            }
            Ok(Self(sid))
        }
    }

    impl Drop for OwnedSid {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }

    /// A security descriptor from `GetNamedSecurityInfoW`, freed on drop.
    struct OwnedDescriptor {
        descriptor: PSECURITY_DESCRIPTOR,
        dacl: *mut ACL,
    }

    impl Drop for OwnedDescriptor {
        fn drop(&mut self) {
            if !self.descriptor.is_null() {
                unsafe { LocalFree(self.descriptor) };
            }
        }
    }

    fn current_dacl(path: &Path) -> SysResult<OwnedDescriptor> {
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("GetNamedSecurityInfoW", status));
        }
        Ok(OwnedDescriptor { descriptor, dacl })
    }

    fn write_dacl(path: &Path, dacl: *mut ACL) -> SysResult<()> {
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide(path).as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("SetNamedSecurityInfoW", status));
        }
        Ok(())
    }

    /// Place one ACE, building the DACL rather than merging through
    /// `SetEntriesInAclW`.
    ///
    /// `SetEntriesInAclW` with `DENY_ACCESS` **replaces** the existing deny
    /// entry for that trustee instead of adding one — measured, not assumed.
    /// [`crate::core::acl::plan_aces`] emits deny-read and deny-write as
    /// two ACEs on the same path for the same trustee, so merging would have
    /// kept only whichever was written last and dropped the other rule
    /// silently, on a DACL that still looked correct.
    ///
    /// Building it directly also removes the dependence on that function's
    /// undocumented ordering, which is the other half of the same problem.
    pub fn place(path: &Path, ace: &PlannedAce, trustee_sid: &str) -> SysResult<()> {
        // Parsed first so a malformed trustee fails before the path is read.
        let _ = OwnedSid::parse(trustee_sid)?;
        let is_directory = path.is_dir();
        let new_entry = AceEntry {
            sid: trustee_sid.to_string(),
            mask: ace.kind.access_mask(),
            is_deny: ace.kind.is_deny(),
            inherited: false,
            flags: ace.ace_flags(is_directory),
        };

        let existing = read_aces(path)?;
        // Re-placing the same rule replaces it rather than duplicating it:
        // `exec` re-applies a session's ACEs on every command, and a DACL
        // that grew an entry each time would eventually stop accepting new
        // ones.
        let mut entries: Vec<AceEntry> = existing
            .into_iter()
            .filter(|entry| !same_rule(entry, &new_entry))
            .collect();
        entries.push(new_entry);

        let ordered = canonical_order(&entries);
        let mut acl = build_acl(&ordered)?;
        write_dacl(path, acl.as_mut_ptr() as *mut ACL)
    }

    /// Whether two entries are the same explicit rule.
    fn same_rule(left: &AceEntry, right: &AceEntry) -> bool {
        !left.inherited
            && left.is_deny == right.is_deny
            && left.mask == right.mask
            && normalise_sid(&left.sid) == normalise_sid(&right.sid)
    }

    /// Explicit denies, explicit allows, then everything inherited.
    ///
    /// Windows evaluates a DACL top down and stops at the first match, so an
    /// allow ahead of a deny makes the deny unreachable — on a DACL that
    /// reads back exactly as written. Inherited entries go last and keep
    /// their relative order: they are the parent's rules, and Windows already
    /// evaluates them after the explicit ones.
    fn canonical_order(entries: &[AceEntry]) -> Vec<&AceEntry> {
        entries
            .iter()
            .filter(|entry| !entry.inherited && entry.is_deny)
            .chain(entries.iter().filter(|e| !e.inherited && !e.is_deny))
            .chain(entries.iter().filter(|e| e.inherited))
            .collect()
    }

    pub fn revoke(path: &Path, kind: AceKind, trustee_sid: &str) -> SysResult<()> {
        let entries = read_aces(path)?;
        let target = normalise_sid(trustee_sid);
        let mask = kind.access_mask();
        let is_deny = kind.is_deny();

        // Matched on (sid, deny-or-allow, exact mask) rather than "any ACE
        // for this trustee": a session that placed a deny-read and a
        // deny-write on one path must be able to remove one without the
        // other, and an ACE somebody else put there for the same trustee is
        // not ours to take off.
        let keep: Vec<&AceEntry> = entries
            .iter()
            .filter(|entry| {
                entry.inherited
                    || !(entry.is_deny == is_deny
                        && entry.mask == mask
                        && normalise_sid(&entry.sid) == target)
            })
            .collect();

        if keep.len() == entries.len() {
            // Nothing matched — already removed, which is a success, not a
            // failure. `reap` runs at the start of every `exec`, and an error
            // here would keep the ledger row alive forever.
            //
            // Returning early also means a revoke that finds nothing does not
            // rewrite the DACL, so it cannot disturb ACEs somebody else owns.
            return Ok(());
        }

        let mut rebuilt = build_acl(&keep)?;
        write_dacl(path, rebuilt.as_mut_ptr() as *mut ACL)
    }

    fn normalise_sid(sid: &str) -> String {
        sid.trim().to_ascii_uppercase()
    }

    pub fn read_aces(path: &Path) -> SysResult<Vec<AceEntry>> {
        let descriptor = current_dacl(path)?;
        if descriptor.dacl.is_null() {
            // A null DACL is "everyone has full access" — not an empty one.
            return Ok(Vec::new());
        }
        let count = unsafe { (*descriptor.dacl).AceCount } as u32;
        let mut entries = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut ace: *mut c_void = null_mut();
            if unsafe { GetAce(descriptor.dacl, index, &mut ace) } == 0 {
                continue;
            }
            let header = ace as *const ACE_HEADER;
            let ace_type = unsafe { (*header).AceType };
            let flags = unsafe { (*header).AceFlags } as u32;
            let (mask, sid_ptr) = match ace_type {
                ACCESS_ALLOWED_ACE_TYPE => {
                    let typed = ace as *const ACCESS_ALLOWED_ACE;
                    (
                        unsafe { (*typed).Mask },
                        unsafe { std::ptr::addr_of!((*typed).SidStart) } as PSID,
                    )
                }
                ACCESS_DENIED_ACE_TYPE => {
                    let typed = ace as *const ACCESS_DENIED_ACE;
                    (
                        unsafe { (*typed).Mask },
                        unsafe { std::ptr::addr_of!((*typed).SidStart) } as PSID,
                    )
                }
                // Audit and alarm ACEs live in the SACL, and object ACEs are
                // for directory services; neither belongs on a file DACL we
                // wrote, and rebuilding one we do not understand would lose
                // information.
                _ => continue,
            };
            entries.push(AceEntry {
                sid: sid_to_string(sid_ptr)?,
                mask,
                is_deny: ace_type == ACCESS_DENIED_ACE_TYPE,
                inherited: flags & INHERITED_ACE != 0,
                flags,
            });
        }
        Ok(entries)
    }

    fn sid_to_string(sid: PSID) -> SysResult<String> {
        let mut text: *mut u16 = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(SysError::win32("ConvertSidToStringSidW", unsafe {
                windows_sys::Win32::Foundation::GetLastError()
            }));
        }
        let mut length = 0usize;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        let rendered =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
        unsafe { LocalFree(text as *mut c_void) };
        Ok(rendered)
    }

    /// Build a fresh ACL from entries, in the order given.
    ///
    /// Returned as a byte buffer the caller owns, so there is no allocation
    /// to free through a Win32 path and no chance of handing
    /// `SetNamedSecurityInfoW` a pointer that has already been released.
    fn build_acl(entries: &[&AceEntry]) -> SysResult<Vec<u8>> {
        let mut sids = Vec::with_capacity(entries.len());
        let mut size = std::mem::size_of::<ACL>();
        for entry in entries {
            let sid = OwnedSid::parse(&entry.sid)?;
            let sid_length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid.0) } as usize;
            // The ACE structs already include four bytes of `SidStart`.
            size += std::mem::size_of::<ACCESS_ALLOWED_ACE>() - 4 + sid_length;
            sids.push((sid, entry));
        }
        // Rounded up: `InitializeAcl` wants a DWORD-aligned size.
        size = size.next_multiple_of(4);

        let mut buffer = vec![0u8; size.max(std::mem::size_of::<ACL>())];
        let acl = buffer.as_mut_ptr() as *mut ACL;
        if unsafe { InitializeAcl(acl, buffer.len() as u32, ACL_REVISION) } == 0 {
            return Err(SysError::win32("InitializeAcl", unsafe {
                windows_sys::Win32::Foundation::GetLastError()
            }));
        }

        for (sid, entry) in &sids {
            let added = if entry.is_deny {
                unsafe { AddAccessDeniedAceEx(acl, ACL_REVISION, entry.flags, entry.mask, sid.0) }
            } else {
                unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, entry.flags, entry.mask, sid.0) }
            };
            if added == 0 {
                return Err(SysError::win32("AddAccessAce", unsafe {
                    windows_sys::Win32::Foundation::GetLastError()
                }));
            }
        }
        Ok(buffer)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::AceEntry;
    use crate::core::acl::{AceKind, PlannedAce};
    use crate::sys::{SysError, SysResult};
    use std::path::Path;

    pub fn place(_path: &Path, _ace: &PlannedAce, _trustee: &str) -> SysResult<()> {
        Err(SysError::Unsupported("writing ACEs"))
    }

    pub fn revoke(_path: &Path, _kind: AceKind, _trustee: &str) -> SysResult<()> {
        Err(SysError::Unsupported("removing ACEs"))
    }

    pub fn read_aces(_path: &Path) -> SysResult<Vec<AceEntry>> {
        Err(SysError::Unsupported("reading ACEs"))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::core::acl::{AceKind, AceOrigin};

    /// `BUILTIN\Guests` — a real, resolvable principal that is not us.
    ///
    /// Deliberately not the current user: a deny ACE for ourselves on the
    /// temp directory would lock the test out of its own fixture.
    const TRUSTEE: &str = "S-1-5-32-546";

    fn ace(kind: AceKind, path: &std::path::Path) -> PlannedAce {
        PlannedAce {
            path: path.to_path_buf(),
            kind,
            origin: match kind {
                AceKind::DenyRead | AceKind::DenyExecute => AceOrigin::DenyRead,
                AceKind::DenyWrite => AceOrigin::DenyWrite,
                AceKind::AllowWrite => AceOrigin::AllowWrite,
            },
        }
    }

    fn ours(entries: &[AceEntry]) -> Vec<&AceEntry> {
        entries
            .iter()
            .filter(|entry| !entry.inherited && entry.sid.eq_ignore_ascii_case(TRUSTEE))
            .collect()
    }

    /// Does the mask actually stop the file running?
    ///
    /// `#[ignore]`, and run by hand — it denies **Everyone**, which includes
    /// whoever is running the suite, on a copy of a system binary in a temp
    /// directory. Everything is undone before it returns and the copy is
    /// thrown away either way, but a test that denies Everyone anything is
    /// not one to leave running unattended in CI.
    ///
    /// Measured rather than reasoned about: the natural argument is "an
    /// image has to be read to be loaded, so denying reads is enough", and
    /// the natural counter-argument is "execution is `FILE_EXECUTE`, so
    /// denying reads proves nothing". Both are plausible and only one
    /// experiment settles it.
    ///
    /// **Result (Windows 10 19044): the spawn fails with access denied.**
    #[test]
    #[ignore = "denies Everyone on a temp copy of a system binary; run by hand"]
    fn a_deny_execute_really_does_prevent_running_the_file() {
        const EVERYONE: &str = "S-1-1-0";
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("hostname.exe");
        std::fs::copy(r"C:\Windows\System32\hostname.exe", &binary).unwrap();

        // It runs before the ACE. Without this the test would pass on a
        // machine where the copy never worked in the first place.
        assert!(
            std::process::Command::new(&binary).output().is_ok(),
            "the copy would not run even before the deny"
        );

        place(&binary, &ace(AceKind::DenyExecute, &binary), EVERYONE).unwrap();
        let blocked = std::process::Command::new(&binary).output();
        revoke(&binary, AceKind::DenyExecute, EVERYONE).unwrap();

        let error = blocked.expect_err("the deny-execute did not stop the process starting");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "stopped, but not by the access check: {error}"
        );

        // And it runs again afterwards, which is what `uninstall` promises.
        assert!(std::process::Command::new(&binary).output().is_ok());
    }

    #[test]
    fn a_deny_execute_lands_on_a_file_and_comes_back_intact() {
        // What `install` writes on the helper's own binary. On a *file*, not
        // a directory: the inherit flags differ, and an ACE written with
        // container inheritance on a file is one Windows reports back
        // differently than it was written.
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("sandbox-win.exe");
        std::fs::write(&binary, b"MZ").unwrap();

        place(&binary, &ace(AceKind::DenyExecute, &binary), TRUSTEE).unwrap();

        let placed = read_aces(&binary).unwrap();
        let mine = ours(&placed);
        assert_eq!(mine.len(), 1, "{placed:?}");
        assert!(mine[0].is_deny);
        assert_eq!(mine[0].mask, AceKind::DenyExecute.access_mask());
        // A file has nothing below it to inherit into.
        assert_eq!(mine[0].flags, 0, "{placed:?}");
    }

    #[test]
    fn a_deny_execute_can_be_taken_back() {
        // `uninstall` has to remove it, and it has to leave the file's other
        // ACEs alone — this runs against a binary the user keeps.
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("sandbox-win.exe");
        std::fs::write(&binary, b"MZ").unwrap();
        let before = read_aces(&binary).unwrap().len();

        place(&binary, &ace(AceKind::DenyExecute, &binary), TRUSTEE).unwrap();
        revoke(&binary, AceKind::DenyExecute, TRUSTEE).unwrap();

        let after = read_aces(&binary).unwrap();
        assert!(ours(&after).is_empty(), "{after:?}");
        assert_eq!(
            after.len(),
            before,
            "an unrelated ACE was disturbed: {after:?}"
        );
    }

    #[test]
    fn revoking_a_deny_execute_that_was_never_placed_is_not_an_error() {
        // The state after an upgrade replaced the binary: `uninstall` must
        // still succeed, having found nothing.
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("sandbox-win.exe");
        std::fs::write(&binary, b"MZ").unwrap();

        revoke(&binary, AceKind::DenyExecute, TRUSTEE).unwrap();
    }

    #[test]
    fn an_ace_can_be_placed_and_read_back() {
        // The property the whole non-elevated design rests on: a user can do
        // this to their own directory, with no administrator anywhere.
        let temp = tempfile::tempdir().unwrap();
        let planned = ace(AceKind::DenyWrite, temp.path());

        place(temp.path(), &planned, TRUSTEE).unwrap();

        let placed = read_aces(temp.path()).unwrap();
        let mine = ours(&placed);
        assert_eq!(mine.len(), 1, "{placed:?}");
        assert!(mine[0].is_deny);
        assert_eq!(mine[0].mask, AceKind::DenyWrite.access_mask());
    }

    #[test]
    fn a_deny_ends_up_ahead_of_a_grant_whatever_order_they_were_placed_in() {
        // The mistake that leaves no evidence: Windows stops at the first
        // matching ACE, so an allow in front of a deny makes the deny
        // unreachable while the DACL reads back exactly as written.
        // `SetEntriesInAclW` does not promise canonical order, so this is
        // checked against the real API rather than assumed.
        let temp = tempfile::tempdir().unwrap();

        place(temp.path(), &ace(AceKind::AllowWrite, temp.path()), TRUSTEE).unwrap();
        place(temp.path(), &ace(AceKind::DenyWrite, temp.path()), TRUSTEE).unwrap();

        let entries = read_aces(temp.path()).unwrap();
        let explicit: Vec<&AceEntry> = entries.iter().filter(|e| !e.inherited).collect();
        let first_allow = explicit.iter().position(|e| !e.is_deny);
        let last_deny = explicit.iter().rposition(|e| e.is_deny);
        if let (Some(allow), Some(deny)) = (first_allow, last_deny) {
            assert!(
                deny < allow,
                "a grant sorted ahead of a denial: {explicit:?}"
            );
        }
    }

    #[test]
    fn revoking_removes_only_the_matching_ace() {
        // A session that placed both a deny-read and a deny-write on one path
        // must be able to take one off without the other.
        let temp = tempfile::tempdir().unwrap();
        place(temp.path(), &ace(AceKind::DenyWrite, temp.path()), TRUSTEE).unwrap();
        place(temp.path(), &ace(AceKind::DenyRead, temp.path()), TRUSTEE).unwrap();
        assert_eq!(ours(&read_aces(temp.path()).unwrap()).len(), 2);

        revoke(temp.path(), AceKind::DenyWrite, TRUSTEE).unwrap();

        let remaining = read_aces(temp.path()).unwrap();
        let mine = ours(&remaining);
        assert_eq!(mine.len(), 1, "{remaining:?}");
        assert_eq!(mine[0].mask, AceKind::DenyRead.access_mask());
    }

    #[test]
    fn revoking_is_idempotent() {
        // `reap` runs at the start of every `exec`; an ACE a previous run
        // already removed has to be a success, not a failure that keeps the
        // ledger row alive forever.
        let temp = tempfile::tempdir().unwrap();
        place(temp.path(), &ace(AceKind::DenyWrite, temp.path()), TRUSTEE).unwrap();

        revoke(temp.path(), AceKind::DenyWrite, TRUSTEE).unwrap();
        revoke(temp.path(), AceKind::DenyWrite, TRUSTEE).unwrap();

        assert!(ours(&read_aces(temp.path()).unwrap()).is_empty());
    }

    #[test]
    fn revoking_something_never_placed_succeeds_and_changes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let before = read_aces(temp.path()).unwrap();

        revoke(temp.path(), AceKind::DenyWrite, TRUSTEE).unwrap();

        assert_eq!(read_aces(temp.path()).unwrap(), before);
    }

    #[test]
    fn other_principals_aces_are_left_alone() {
        // The user's own access to their own directory must survive both
        // operations — an ACL rebuild that dropped inherited entries would
        // lock them out of their project.
        let temp = tempfile::tempdir().unwrap();
        let before = read_aces(temp.path()).unwrap();
        let others_before: Vec<&AceEntry> = before
            .iter()
            .filter(|e| !e.sid.eq_ignore_ascii_case(TRUSTEE))
            .collect();

        place(temp.path(), &ace(AceKind::DenyWrite, temp.path()), TRUSTEE).unwrap();
        revoke(temp.path(), AceKind::DenyWrite, TRUSTEE).unwrap();

        let after = read_aces(temp.path()).unwrap();
        let others_after: Vec<&AceEntry> = after
            .iter()
            .filter(|e| !e.sid.eq_ignore_ascii_case(TRUSTEE))
            .collect();
        assert_eq!(others_before.len(), others_after.len(), "{after:?}");
    }

    #[test]
    fn a_directory_ace_inherits_and_a_file_ace_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();

        place(temp.path(), &ace(AceKind::DenyWrite, temp.path()), TRUSTEE).unwrap();
        place(&file, &ace(AceKind::DenyWrite, &file), TRUSTEE).unwrap();

        let directory_ace = ours(&read_aces(temp.path()).unwrap())[0].flags;
        let file_entries = read_aces(&file).unwrap();
        let file_ace = ours(&file_entries)[0].flags;

        assert_ne!(
            directory_ace & 0x03,
            0,
            "a directory rule must reach inside it"
        );
        assert_eq!(file_ace & 0x03, 0, "a file has nothing below it to inherit");
    }

    #[test]
    fn a_path_that_does_not_exist_is_an_error_rather_than_a_silent_success() {
        let temp = tempfile::tempdir().unwrap();
        let absent = temp.path().join("never-created");

        assert!(place(&absent, &ace(AceKind::DenyWrite, &absent), TRUSTEE).is_err());
        assert!(read_aces(&absent).is_err());
    }

    #[test]
    fn a_malformed_trustee_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let error = place(
            temp.path(),
            &ace(AceKind::DenyWrite, temp.path()),
            "not-a-sid",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("ConvertStringSidToSidW"),
            "{error}"
        );
    }
}
