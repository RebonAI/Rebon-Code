//! Looking up the SIDs the rest of the helper is written against.
//!
//! Three principals matter: the two downgraded accounts, whose SIDs key the WFP
//! filters, and the local group they share, whose SID goes on every ACE. Nothing
//! here creates anything — `LookupAccountNameW` is a read, so `status` can call
//! it without elevating.
//!
//! Names are resolved on the local machine only (`lpSystemName` is null). A
//! domain lookup would be slow when the domain is unreachable, and a domain
//! account with a colliding name is emphatically not the account we created.

use crate::sys::SysResult;

/// Which of the helper's principals exist.
///
/// Reported as a set rather than a boolean because a half-finished install — one
/// account created, the other not — needs to be visible as such rather than as
/// "not installed", which would send the user to run `install` again over a
/// partial state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountProbe {
    pub user: Option<String>,
    pub user_no_network: Option<String>,
    pub group: Option<String>,
}

impl AccountProbe {
    /// Every principal exists — the `user=ok` probe in `status`.
    pub fn is_complete(&self) -> bool {
        self.user.is_some() && self.user_no_network.is_some() && self.group.is_some()
    }

    /// Some but not all — an interrupted `install`.
    pub fn is_partial(&self) -> bool {
        !self.is_complete()
            && (self.user.is_some() || self.user_no_network.is_some() || self.group.is_some())
    }

    /// The principals that are missing, by name, for a remediation line.
    pub fn missing(&self) -> Vec<&'static str> {
        use crate::core::account::{SANDBOX_GROUP, SANDBOX_USER, SANDBOX_USER_NO_NETWORK};
        let mut missing = Vec::new();
        if self.user.is_none() {
            missing.push(SANDBOX_USER);
        }
        if self.user_no_network.is_none() {
            missing.push(SANDBOX_USER_NO_NETWORK);
        }
        if self.group.is_none() {
            missing.push(SANDBOX_GROUP);
        }
        missing
    }
}

/// The SID of a local account or group, as a string.
///
/// `Ok(None)` means the name does not exist, which is an answer rather than a
/// failure — it is what `status` reports before `install` has run.
pub fn lookup_sid(name: &str) -> SysResult<Option<String>> {
    imp::lookup_sid(name)
}

/// The SID of the user running this process.
pub fn current_user_sid() -> SysResult<String> {
    imp::current_user_sid()
}

/// Look up all three of the helper's principals.
pub fn probe_accounts() -> SysResult<AccountProbe> {
    use crate::core::account::{SANDBOX_GROUP, SANDBOX_USER, SANDBOX_USER_NO_NETWORK};
    Ok(AccountProbe {
        user: lookup_sid(SANDBOX_USER)?,
        user_no_network: lookup_sid(SANDBOX_USER_NO_NETWORK)?,
        group: lookup_sid(SANDBOX_GROUP)?,
    })
}

#[cfg(windows)]
mod imp {
    use crate::sys::{SysError, SysResult};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_NONE_MAPPED, HANDLE, PSID,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, LookupAccountNameW, TokenUser, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub fn lookup_sid(name: &str) -> SysResult<Option<String>> {
        let wide: Vec<u16> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(Some(0))
            .collect();

        // Two-pass: the first call reports the sizes it needs.
        let mut sid_length = 0u32;
        let mut domain_length = 0u32;
        let mut use_kind: SID_NAME_USE = 0;
        unsafe {
            LookupAccountNameW(
                null(),
                wide.as_ptr(),
                null_mut(),
                &mut sid_length,
                null_mut(),
                &mut domain_length,
                &mut use_kind,
            )
        };
        let code = unsafe { GetLastError() };
        if sid_length == 0 {
            if code == ERROR_NONE_MAPPED || code == 1332 {
                return Ok(None);
            }
            return Err(SysError::win32("LookupAccountNameW", code));
        }

        let mut sid = vec![0u8; sid_length as usize];
        let mut domain = vec![0u16; domain_length.max(1) as usize];
        let ok = unsafe {
            LookupAccountNameW(
                null(),
                wide.as_ptr(),
                sid.as_mut_ptr() as PSID,
                &mut sid_length,
                domain.as_mut_ptr(),
                &mut domain_length,
                &mut use_kind,
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_NONE_MAPPED || code == 1332 {
                return Ok(None);
            }
            return Err(SysError::win32("LookupAccountNameW", code));
        }

        sid_to_string(sid.as_ptr() as PSID).map(Some)
    }

    pub fn current_user_sid() -> SysResult<String> {
        let mut token: HANDLE = 0;
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(SysError::win32("OpenProcessToken", unsafe {
                GetLastError()
            }));
        }

        let mut needed = 0u32;
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed) };
        if needed == 0 {
            let code = unsafe { GetLastError() };
            unsafe { CloseHandle(token) };
            return Err(SysError::win32("GetTokenInformation", code));
        }

        let mut buffer = vec![0u8; needed as usize];
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        };
        let code = unsafe { GetLastError() };
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(SysError::win32("GetTokenInformation", code));
        }

        let user = buffer.as_ptr() as *const TOKEN_USER;
        sid_to_string(unsafe { (*user).User.Sid })
    }

    fn sid_to_string(sid: PSID) -> SysResult<String> {
        let mut text: *mut u16 = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(SysError::win32("ConvertSidToStringSidW", unsafe {
                GetLastError()
            }));
        }
        let mut length = 0usize;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(text, length) };
        let rendered = String::from_utf16_lossy(slice);
        unsafe { LocalFree(text as *mut c_void) };
        Ok(rendered)
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::sys::{SysError, SysResult};

    pub fn lookup_sid(_name: &str) -> SysResult<Option<String>> {
        Err(SysError::Unsupported("SID lookup"))
    }

    pub fn current_user_sid() -> SysResult<String> {
        Err(SysError::Unsupported("SID lookup"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_probe_reports_nothing_missing() {
        let probe = AccountProbe {
            user: Some("S-1-5-21-1-2-3-1001".into()),
            user_no_network: Some("S-1-5-21-1-2-3-1002".into()),
            group: Some("S-1-5-21-1-2-3-1004".into()),
        };
        assert!(probe.is_complete());
        assert!(!probe.is_partial());
        assert!(probe.missing().is_empty());
    }

    #[test]
    fn an_empty_probe_is_neither_complete_nor_partial() {
        // "Nothing installed" and "half installed" need different words: one says run
        // `install`, the other says something went wrong partway through and the state on
        // the machine is not what either side thinks.
        let probe = AccountProbe::default();
        assert!(!probe.is_complete());
        assert!(!probe.is_partial());
        assert_eq!(probe.missing().len(), 3);
    }

    #[test]
    fn a_half_finished_install_is_visible_as_partial() {
        let probe = AccountProbe {
            user: Some("S-1-5-21-1-2-3-1001".into()),
            ..Default::default()
        };
        assert!(probe.is_partial());
        assert_eq!(
            probe.missing(),
            vec![
                crate::core::account::SANDBOX_USER_NO_NETWORK,
                crate::core::account::SANDBOX_GROUP
            ]
        );
    }

    #[test]
    fn a_missing_group_alone_still_blocks_readiness() {
        // Every ACE names the group. Two accounts with no group to put the ACEs on is not
        // a usable sandbox, however complete it looks.
        let probe = AccountProbe {
            user: Some("S-1-5-21-1-2-3-1001".into()),
            user_no_network: Some("S-1-5-21-1-2-3-1002".into()),
            group: None,
        };
        assert!(!probe.is_complete());
        assert_eq!(probe.missing(), vec![crate::core::account::SANDBOX_GROUP]);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn a_well_known_account_resolves_to_its_well_known_sid() {
        // SYSTEM is S-1-5-18 on every Windows machine, so this checks the lookup and the
        // SID-to-string conversion without depending on anything the helper installed.
        let sid = lookup_sid("SYSTEM")
            .unwrap()
            .expect("SYSTEM must resolve on any Windows machine");
        assert_eq!(sid, "S-1-5-18");
    }

    #[test]
    fn a_name_that_does_not_exist_is_none_rather_than_an_error() {
        // This is the state `status` reports before `install` has ever run, and it must
        // not look like a failure to probe.
        assert_eq!(
            lookup_sid("sandbox-win-no-such-account-9f3a").unwrap(),
            None
        );
    }

    #[test]
    fn the_current_user_has_a_well_formed_sid() {
        let sid = current_user_sid().unwrap();
        assert!(
            crate::core::sddl::is_sid(&sid),
            "{sid} is not a SID the SDDL builder would accept"
        );
    }

    #[test]
    fn probing_the_accounts_needs_no_elevation_and_reports_absence() {
        // `status` must be a non-elevated read. Before `install` this is simply an empty
        // probe, and getting that far without an error is the assertion.
        let probe = probe_accounts().unwrap();
        if !probe.is_complete() {
            assert!(!probe.missing().is_empty());
        }
    }
}
