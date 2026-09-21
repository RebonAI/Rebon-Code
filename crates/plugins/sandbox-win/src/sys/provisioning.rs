//! Creating and removing the sandbox accounts.
//!
//! Everything here needs administrator rights and runs exactly twice in a
//! machine's life: once at `install`, once at `uninstall`.
//!
//! ## Both directions are idempotent
//!
//! Not a convenience. A half-finished install is the one state this system has no
//! good answer for — `status` reports neither installed nor clean, and the user
//! cannot tell which half is missing. So every step here checks first and
//! converges: `install` over a partial state finishes it, and `uninstall` over a
//! clean machine succeeds having done nothing.
//!
//! Re-running `install` also **resets the passwords**. The stored credential is
//! the only copy; if it was lost, the account is unusable and there is no way to
//! recover the old password. Resetting is the only thing that can make an existing
//! account work again.
//!
//! ## The logon right, and why it is granted rather than denied
//!
//! Denying interactive logon was the original plan. The logon type
//! `CreateProcessWithLogonW` actually performs was measured, and it is
//! **Interactive** — so denying it stops the sandbox working entirely. It is
//! granted explicitly here, which is also required because the install removes the
//! accounts from `Users`, and `Users` is where that right comes from by default.
//! The two changes together, without this, leave an account that cannot log on at
//! all (`ERROR_LOGON_TYPE_NOT_GRANTED`).
//!
//! Everything *else* is denied: network, remote-interactive, service, batch.

use crate::sys::SysResult;

/// Rights the sandbox accounts must have and must not have.
pub mod rights {
    /// The one to keep — see the module docs.
    pub const INTERACTIVE: &str = "SeInteractiveLogonRight";

    /// The ones to deny. A `SeDeny*` right beats the corresponding grant, so
    /// these hold even if something later adds the account to a group that
    /// would have allowed it.
    pub const DENIED: &[&str] = &[
        "SeDenyNetworkLogonRight",
        "SeDenyRemoteInteractiveLogonRight",
        "SeDenyServiceLogonRight",
        "SeDenyBatchLogonRight",
    ];
}

/// What `install` did, so it can be reported rather than guessed at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    pub group_created: bool,
    pub accounts_created: Vec<String>,
    pub accounts_reset: Vec<String>,
    pub notes: Vec<String>,
}

/// What `uninstall` removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemovalReport {
    pub accounts_removed: Vec<String>,
    pub profiles_removed: Vec<String>,
    pub group_removed: bool,
    pub notes: Vec<String>,
}

/// Create the group, both accounts, and their rights.
///
/// `passwords` is `(account, password)` for each account, already generated
/// by the caller so the credential store and the accounts cannot disagree
/// about what was set.
pub fn provision(passwords: &[(&str, &str)]) -> SysResult<ProvisionReport> {
    imp::provision(passwords)
}

/// Remove the accounts, their profiles, and the group.
pub fn deprovision() -> SysResult<RemovalReport> {
    imp::deprovision()
}

/// Whether a local account or group exists.
pub fn account_exists(name: &str) -> SysResult<bool> {
    imp::account_exists(name)
}

#[cfg(windows)]
mod imp {
    use super::{rights, ProvisionReport, RemovalReport};
    use crate::sys::{SysError, SysResult};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use crate::core::account::{SANDBOX_GROUP, SANDBOX_USER, SANDBOX_USER_NO_NETWORK};
    use windows_sys::Win32::Foundation::PSID;
    use windows_sys::Win32::NetworkManagement::NetManagement::NERR_Success as NET_SUCCESS;
    use windows_sys::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, NetLocalGroupAdd, NetLocalGroupAddMembers, NetLocalGroupDel,
        NetLocalGroupDelMembers, NetUserAdd, NetUserDel, NetUserGetInfo, NetUserSetInfo,
        LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_3, UF_DONT_EXPIRE_PASSWD, UF_PASSWD_CANT_CHANGE,
        UF_SCRIPT, USER_INFO_1, USER_INFO_1003, USER_PRIV_USER,
    };
    use windows_sys::Win32::Security::Authentication::Identity::{
        LsaAddAccountRights, LsaClose, LsaOpenPolicy, LsaRemoveAccountRights,
        LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
    };

    /// `NERR_UserNotFound`
    const NERR_USER_NOT_FOUND: u32 = 2221;
    /// `NERR_GroupNotFound`
    const NERR_GROUP_NOT_FOUND: u32 = 2220;
    /// `NERR_UserExists`
    const NERR_USER_EXISTS: u32 = 2224;
    /// `NERR_GroupExists`
    const NERR_GROUP_EXISTS: u32 = 2223;
    /// `ERROR_ALIAS_EXISTS` — what `NetLocalGroupAdd` *actually* returns for
    /// a group that is already there.
    ///
    /// Measured, on the second real `install`. The documented `NERR_*` code
    /// is not the one that comes back, and accepting only that one made the
    /// module's own "install is idempotent" claim false the first time it was
    /// put to the test — a partial install could not be finished by running
    /// install again, which is the one thing it exists to do.
    const ERROR_ALIAS_EXISTS: u32 = 1379;
    /// `ERROR_USER_EXISTS`, the same trap on the account side.
    const ERROR_USER_EXISTS: u32 = 1316;
    /// `ERROR_MEMBER_IN_ALIAS`
    const ERROR_MEMBER_IN_ALIAS: u32 = 1378;
    /// `ERROR_MEMBER_NOT_IN_ALIAS`
    const ERROR_MEMBER_NOT_IN_ALIAS: u32 = 1377;
    /// The well-known `BUILTIN\Users` alias.
    const USERS_GROUP_SID: &str = "S-1-5-32-545";

    /// Written into every account this helper creates, and checked before
    /// deleting one.
    ///
    /// The names are ours; the machine is not. An account already called
    /// `rebon-sbx` that somebody else made is not something `uninstall` may
    /// delete, and deleting a person's account is not a recoverable mistake.
    /// So the check is on the way out as well as the way in — the same guard
    /// the teardown script carries, for the same reason.
    pub(super) const ACCOUNT_MARKER: &str =
        "Rebon sandbox account. Safe to delete via sandbox-win uninstall.";

    /// Whether the account carries our marker.
    ///
    /// An account whose comment cannot be read is left alone: not being able
    /// to tell is a reason to stop, not to proceed.
    fn is_ours(account: &str) -> bool {
        let mut buffer: *mut u8 = null_mut();
        let status = unsafe { NetUserGetInfo(null(), wide(account).as_ptr(), 1, &mut buffer) };
        if status != NET_SUCCESS || buffer.is_null() {
            return false;
        }
        let info = buffer as *const USER_INFO_1;
        let comment = unsafe { (*info).usri1_comment };
        let mut owned = false;
        if !comment.is_null() {
            let mut length = 0usize;
            while unsafe { *comment.add(length) } != 0 {
                length += 1;
            }
            let text =
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(comment, length) });
            owned = text == ACCOUNT_MARKER;
        }
        unsafe { NetApiBufferFree(buffer as *mut std::ffi::c_void) };
        owned
    }

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    pub fn account_exists(name: &str) -> SysResult<bool> {
        let mut buffer: *mut u8 = null_mut();
        let status = unsafe { NetUserGetInfo(null(), wide(name).as_ptr(), 0, &mut buffer) };
        if !buffer.is_null() {
            unsafe { NetApiBufferFree(buffer as *mut std::ffi::c_void) };
        }
        match status {
            NET_SUCCESS => Ok(true),
            NERR_USER_NOT_FOUND => Ok(false),
            other => Err(SysError::win32("NetUserGetInfo", other)),
        }
    }

    fn create_group(report: &mut ProvisionReport) -> SysResult<()> {
        let mut name = wide(SANDBOX_GROUP);
        let mut comment = wide("Rebon sandbox accounts. Created by sandbox-win.exe install.");
        let info = LOCALGROUP_INFO_1 {
            lgrpi1_name: name.as_mut_ptr(),
            lgrpi1_comment: comment.as_mut_ptr(),
        };
        let status =
            unsafe { NetLocalGroupAdd(null(), 1, &info as *const _ as *const u8, null_mut()) };
        match status {
            NET_SUCCESS => {
                report.group_created = true;
                Ok(())
            }
            // Already there. `install` converges rather than refusing, so a
            // half-finished run can be completed by running it again.
            NERR_GROUP_EXISTS | ERROR_ALIAS_EXISTS => Ok(()),
            other => Err(SysError::win32("NetLocalGroupAdd", other)),
        }
    }

    fn create_account(
        account: &str,
        password: &str,
        report: &mut ProvisionReport,
    ) -> SysResult<()> {
        let mut name = wide(account);
        let mut secret = wide(password);
        let mut comment = wide(ACCOUNT_MARKER);
        let info = USER_INFO_1 {
            usri1_name: name.as_mut_ptr(),
            usri1_password: secret.as_mut_ptr(),
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: null_mut(),
            usri1_comment: comment.as_mut_ptr(),
            // `UF_SCRIPT` is required on every account NetUserAdd creates.
            usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE,
            usri1_script_path: null_mut(),
        };

        let status = unsafe { NetUserAdd(null(), 1, &info as *const _ as *const u8, null_mut()) };
        match status {
            NET_SUCCESS => {
                report.accounts_created.push(account.to_string());
                Ok(())
            }
            NERR_USER_EXISTS | ERROR_USER_EXISTS => {
                // The stored credential is the only copy of the password. If
                // it was lost, the account cannot be logged into and there is
                // no way to recover the old one — resetting is the only thing
                // that makes an existing account usable again.
                let mut secret = wide(password);
                let reset = USER_INFO_1003 {
                    usri1003_password: secret.as_mut_ptr(),
                };
                let status = unsafe {
                    NetUserSetInfo(
                        null(),
                        wide(account).as_ptr(),
                        1003,
                        &reset as *const _ as *const u8,
                        null_mut(),
                    )
                };
                if status != NET_SUCCESS {
                    return Err(SysError::win32("NetUserSetInfo", status));
                }
                report.accounts_reset.push(account.to_string());
                Ok(())
            }
            other => Err(SysError::win32("NetUserAdd", other)),
        }
    }

    fn set_membership(account: &str, report: &mut ProvisionReport) -> SysResult<()> {
        let qualified = wide(account);
        let member = LOCALGROUP_MEMBERS_INFO_3 {
            lgrmi3_domainandname: qualified.as_ptr() as *mut u16,
        };
        let status = unsafe {
            NetLocalGroupAddMembers(
                null(),
                wide(SANDBOX_GROUP).as_ptr(),
                3,
                &member as *const _ as *const u8,
                1,
            )
        };
        if status != NET_SUCCESS && status != ERROR_MEMBER_IN_ALIAS {
            return Err(SysError::win32("NetLocalGroupAddMembers", status));
        }

        // out of `Users`, or the account inherits read access to a
        // great deal of the user's profile by default. Looked up by SID
        // because the built-in group's *name* is localised.
        let users = super::group_name_for_sid(USERS_GROUP_SID)?;
        let status = unsafe {
            NetLocalGroupDelMembers(
                null(),
                wide(&users).as_ptr(),
                3,
                &member as *const _ as *const u8,
                1,
            )
        };
        match status {
            NET_SUCCESS => {}
            // Already out, which is where we want it.
            ERROR_MEMBER_NOT_IN_ALIAS => {}
            other => {
                report.notes.push(format!(
                    "could not remove {account} from {users} ({}); it keeps the default read \
                     access that membership grants",
                    SysError::win32("NetLocalGroupDelMembers", other)
                ));
            }
        }
        Ok(())
    }

    /// An open LSA policy handle, closed on drop.
    struct Policy(isize);

    impl Policy {
        fn open() -> SysResult<Self> {
            let attributes: LSA_OBJECT_ATTRIBUTES = unsafe { std::mem::zeroed() };
            let mut handle: isize = 0;
            let status = unsafe {
                LsaOpenPolicy(
                    null(),
                    &attributes,
                    (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32,
                    &mut handle,
                )
            };
            if status != 0 {
                return Err(SysError::win32("LsaOpenPolicy", status as u32));
            }
            Ok(Self(handle))
        }
    }

    impl Drop for Policy {
        fn drop(&mut self) {
            unsafe { LsaClose(self.0) };
        }
    }

    fn lsa_string(buffer: &mut Vec<u16>) -> LSA_UNICODE_STRING {
        // Length is in *bytes* and excludes the terminator; MaximumLength
        // includes it. Getting this wrong truncates the right's name, and
        // the call then silently grants a different right or none.
        let characters = buffer.len().saturating_sub(1);
        LSA_UNICODE_STRING {
            Length: (characters * 2) as u16,
            MaximumLength: (buffer.len() * 2) as u16,
            Buffer: buffer.as_mut_ptr(),
        }
    }

    fn apply_rights(account: &str, report: &mut ProvisionReport) -> SysResult<()> {
        let policy = Policy::open()?;
        let sid = super::sid_for_account(account)?;

        let mut granted = wide(rights::INTERACTIVE);
        let grant = [lsa_string(&mut granted)];
        let status = unsafe { LsaAddAccountRights(policy.0, sid.raw(), grant.as_ptr(), 1) };
        if status != 0 {
            return Err(SysError::win32("LsaAddAccountRights", status as u32));
        }

        let mut denied: Vec<Vec<u16>> = rights::DENIED.iter().map(|r| wide(r)).collect();
        let strings: Vec<LSA_UNICODE_STRING> = denied.iter_mut().map(lsa_string).collect();
        let status = unsafe {
            LsaAddAccountRights(policy.0, sid.raw(), strings.as_ptr(), strings.len() as u32)
        };
        if status != 0 {
            report.notes.push(format!(
                "could not deny the other logon types for {account} ({}); it can still be \
                 logged into over the network or as a service",
                SysError::win32("LsaAddAccountRights", status as u32)
            ));
        }
        Ok(())
    }

    fn drop_rights(account: &str, report: &mut RemovalReport) {
        let Ok(policy) = Policy::open() else { return };
        let Ok(sid) = super::sid_for_account(account) else {
            return;
        };
        let mut all: Vec<Vec<u16>> = std::iter::once(wide(rights::INTERACTIVE))
            .chain(rights::DENIED.iter().map(|r| wide(r)))
            .collect();
        let strings: Vec<LSA_UNICODE_STRING> = all.iter_mut().map(lsa_string).collect();
        let status = unsafe {
            LsaRemoveAccountRights(
                policy.0,
                sid.raw(),
                0,
                strings.as_ptr(),
                strings.len() as u32,
            )
        };
        if status != 0 {
            // Deleting the account removes its rights anyway; this is
            // belt-and-braces for the case where the account survives.
            report.notes.push(format!(
                "could not clear the logon rights for {account} before deleting it ({})",
                SysError::win32("LsaRemoveAccountRights", status as u32)
            ));
        }
    }

    pub fn provision(passwords: &[(&str, &str)]) -> SysResult<ProvisionReport> {
        let mut report = ProvisionReport::default();
        create_group(&mut report)?;
        for (account, password) in passwords {
            create_account(account, password, &mut report)?;
            set_membership(account, &mut report)?;
            apply_rights(account, &mut report)?;
            super::hide_from_logon_screen(account, &mut report.notes);
            super::create_profile(account, &mut report.notes);
        }
        Ok(report)
    }

    pub fn deprovision() -> SysResult<RemovalReport> {
        let mut report = RemovalReport::default();

        for account in [SANDBOX_USER, SANDBOX_USER_NO_NETWORK] {
            if !account_exists(account).unwrap_or(false) {
                continue;
            }
            if !is_ours(account) {
                report.notes.push(format!(
                    "refusing to delete {account}: it exists but was not created by `sandbox-win.exe install`, so it belongs to someone else"
                ));
                continue;
            }
            drop_rights(account, &mut report);
            super::unhide_from_logon_screen(account, &mut report.notes);
            // Before the account is deleted, while its SID still resolves.
            // `NetUserDel` removes the account and leaves `C:\Users\<name>`
            // and its `ProfileList` entry behind — an orphan whose SID names
            // nobody, and two strange directories in the user's `C:\Users`.
            if super::delete_profile(account, &mut report.notes) {
                report.profiles_removed.push(account.to_string());
            }

            let status = unsafe { NetUserDel(null(), wide(account).as_ptr()) };
            match status {
                NET_SUCCESS => report.accounts_removed.push(account.to_string()),
                NERR_USER_NOT_FOUND => {}
                other => {
                    return Err(SysError::win32("NetUserDel", other));
                }
            }
        }

        let status = unsafe { NetLocalGroupDel(null(), wide(SANDBOX_GROUP).as_ptr()) };
        match status {
            NET_SUCCESS => report.group_removed = true,
            NERR_GROUP_NOT_FOUND => {}
            other => return Err(SysError::win32("NetLocalGroupDel", other)),
        }

        Ok(report)
    }

    /// An owned SID buffer, so the pointer outlives the call that uses it.
    pub struct AccountSid(Vec<u8>);

    impl AccountSid {
        pub fn new(buffer: Vec<u8>) -> Self {
            Self(buffer)
        }

        pub fn raw(&self) -> PSID {
            self.0.as_ptr() as PSID
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{ProvisionReport, RemovalReport};
    use crate::sys::{SysError, SysResult};

    pub fn provision(_passwords: &[(&str, &str)]) -> SysResult<ProvisionReport> {
        Err(SysError::Unsupported("account provisioning"))
    }

    pub fn deprovision() -> SysResult<RemovalReport> {
        Err(SysError::Unsupported("account provisioning"))
    }

    pub fn account_exists(_name: &str) -> SysResult<bool> {
        Err(SysError::Unsupported("account lookup"))
    }
}

#[cfg(windows)]
pub(crate) use platform::*;

#[cfg(windows)]
mod platform {
    use super::imp::AccountSid;
    use crate::sys::{SysError, SysResult};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{LookupAccountNameW, LookupAccountSidW, SID_NAME_USE};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    };
    use windows_sys::Win32::UI::Shell::{CreateProfile, DeleteProfileW};

    /// Where Windows looks for accounts to hide from the sign-in screen.
    const SPECIAL_ACCOUNTS: &str =
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn from_wide(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }

    /// The SID of a local account, as a buffer we own.
    pub(crate) fn sid_for_account(name: &str) -> SysResult<AccountSid> {
        let wide_name = wide(name);
        let mut sid_length = 0u32;
        let mut domain_length = 0u32;
        let mut kind: SID_NAME_USE = 0;
        unsafe {
            LookupAccountNameW(
                null(),
                wide_name.as_ptr(),
                null_mut(),
                &mut sid_length,
                null_mut(),
                &mut domain_length,
                &mut kind,
            )
        };
        if sid_length == 0 {
            return Err(SysError::win32("LookupAccountNameW", unsafe {
                windows_sys::Win32::Foundation::GetLastError()
            }));
        }
        let mut sid = vec![0u8; sid_length as usize];
        let mut domain = vec![0u16; domain_length.max(1) as usize];
        let ok = unsafe {
            LookupAccountNameW(
                null(),
                wide_name.as_ptr(),
                sid.as_mut_ptr() as *mut std::ffi::c_void,
                &mut sid_length,
                domain.as_mut_ptr(),
                &mut domain_length,
                &mut kind,
            )
        };
        if ok == 0 {
            return Err(SysError::win32("LookupAccountNameW", unsafe {
                windows_sys::Win32::Foundation::GetLastError()
            }));
        }
        Ok(AccountSid::new(sid))
    }

    /// The local name of a well-known group, from its SID.
    ///
    /// `BUILTIN\Users` is `Users` on an English install and something else
    /// on every other one; the SID is the only stable handle. This machine's
    /// own comments are in Chinese, which is exactly the case a hard-coded
    /// English literal gets wrong.
    pub(crate) fn group_name_for_sid(sid_text: &str) -> SysResult<String> {
        let mut sid: *mut std::ffi::c_void = null_mut();
        if unsafe { ConvertStringSidToSidW(wide(sid_text).as_ptr(), &mut sid) } == 0 {
            return Err(SysError::win32("ConvertStringSidToSidW", unsafe {
                windows_sys::Win32::Foundation::GetLastError()
            }));
        }
        let mut name = vec![0u16; 256];
        let mut name_length = name.len() as u32;
        let mut domain = vec![0u16; 256];
        let mut domain_length = domain.len() as u32;
        let mut kind: SID_NAME_USE = 0;
        let ok = unsafe {
            LookupAccountSidW(
                null(),
                sid,
                name.as_mut_ptr(),
                &mut name_length,
                domain.as_mut_ptr(),
                &mut domain_length,
                &mut kind,
            )
        };
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        unsafe { windows_sys::Win32::Foundation::LocalFree(sid) };
        if ok == 0 {
            return Err(SysError::win32("LookupAccountSidW", code));
        }
        Ok(from_wide(&name))
    }

    fn open_special_accounts() -> SysResult<HKEY> {
        let mut key: HKEY = 0;
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                wide(SPECIAL_ACCOUNTS).as_ptr(),
                0,
                null_mut(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null_mut(),
                &mut key,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(SysError::win32("RegCreateKeyExW", status));
        }
        Ok(key)
    }

    /// Hide an account from the sign-in screen.
    ///
    /// Not cosmetic. Without it the user reboots and finds two accounts they
    /// have never seen on their own machine, and concluding they have been
    /// compromised is the reasonable response.
    ///
    /// A note rather than an error: a hidden account is nicer, an unhidden
    /// one still works, and failing the whole install over it would trade a
    /// working sandbox for a tidy login screen.
    pub(crate) fn hide_from_logon_screen(account: &str, notes: &mut Vec<String>) {
        let key = match open_special_accounts() {
            Ok(key) => key,
            Err(error) => {
                notes.push(format!(
                    "could not hide {account} from the sign-in screen: {error}"
                ));
                return;
            }
        };
        let zero: u32 = 0;
        let status = unsafe {
            RegSetValueExW(
                key,
                wide(account).as_ptr(),
                0,
                REG_DWORD,
                &zero as *const u32 as *const u8,
                std::mem::size_of::<u32>() as u32,
            )
        };
        unsafe { RegCloseKey(key) };
        if status != ERROR_SUCCESS {
            notes.push(format!(
                "could not hide {account} from the sign-in screen: {}",
                SysError::win32("RegSetValueExW", status)
            ));
        }
    }

    pub(crate) fn unhide_from_logon_screen(account: &str, notes: &mut Vec<String>) {
        let Ok(key) = open_special_accounts() else {
            return;
        };
        let status = unsafe { RegDeleteValueW(key, wide(account).as_ptr()) };
        unsafe { RegCloseKey(key) };
        // 2 is "no such value", which is where we want to end up.
        if status != ERROR_SUCCESS && status != 2 {
            notes.push(format!(
                "left a sign-in-screen entry behind for {account}: {}",
                SysError::win32("RegDeleteValueW", status)
            ));
        }
    }

    /// Pre-create the account's profile.
    ///
    /// Creating a profile on first logon takes seconds, and those seconds
    /// land on the user's first sandboxed command as an unexplained timeout.
    pub(crate) fn create_profile(account: &str, notes: &mut Vec<String>) {
        let Ok(sid) = sid_for_account(account) else {
            return;
        };
        let mut sid_text: *mut u16 = null_mut();
        if unsafe {
            windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(
                sid.raw(),
                &mut sid_text,
            )
        } == 0
        {
            return;
        }
        let mut path = vec![0u16; 260];
        let result = unsafe {
            CreateProfile(
                sid_text,
                wide(account).as_ptr(),
                path.as_mut_ptr(),
                path.len() as u32,
            )
        };
        unsafe { windows_sys::Win32::Foundation::LocalFree(sid_text as *mut std::ffi::c_void) };
        // `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)` — already there, which
        // is the desired end state.
        const ALREADY_EXISTS: i32 = -2147024713;
        if result != 0 && result != ALREADY_EXISTS {
            notes.push(format!(
                "could not pre-create the profile for {account} (HRESULT {result:#x}); the \
                 first sandboxed command will pay for it instead"
            ));
        }
    }

    /// Remove the account's profile directory and its registry entry.
    ///
    /// Returns whether anything was removed. `NetUserDel` does **not** do
    /// this: it deletes the account and leaves `C:\Users\<name>` plus the
    /// `ProfileList` key behind, as an orphan whose SID resolves to nobody.
    /// Two of those in `C:\Users` is exactly the "what is this doing on my
    /// machine" that hiding the accounts was meant to avoid.
    pub(crate) fn delete_profile(account: &str, notes: &mut Vec<String>) -> bool {
        let Ok(sid) = sid_for_account(account) else {
            return false;
        };
        let mut sid_text: *mut u16 = null_mut();
        if unsafe {
            windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(
                sid.raw(),
                &mut sid_text,
            )
        } == 0
        {
            return false;
        }
        let removed = unsafe { DeleteProfileW(sid_text, null(), null()) } != 0;
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        unsafe { windows_sys::Win32::Foundation::LocalFree(sid_text as *mut std::ffi::c_void) };
        // 2 is "the profile was never created", which is a clean state.
        if !removed && code != 2 {
            notes.push(format!(
                "left the profile directory for {account} behind: {}",
                SysError::win32("DeleteProfileW", code)
            ));
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interactive_right_is_kept_and_the_rest_denied() {
        // Measurement showed `CreateProcessWithLogonW` performing an
        // *interactive* logon, so denying that right — which the first plan
        // originally said to do — stops the sandbox working at all.
        assert_eq!(rights::INTERACTIVE, "SeInteractiveLogonRight");
        assert!(!rights::DENIED.contains(&"SeDenyInteractiveLogonRight"));
    }

    #[test]
    fn every_other_logon_type_is_denied() {
        for expected in [
            "SeDenyNetworkLogonRight",
            "SeDenyRemoteInteractiveLogonRight",
            "SeDenyServiceLogonRight",
            "SeDenyBatchLogonRight",
        ] {
            assert!(
                rights::DENIED.contains(&expected),
                "{expected} is not denied"
            );
        }
    }

    #[test]
    fn the_denied_rights_are_all_deny_rights() {
        // A grant accidentally listed here would hand the account a logon
        // type instead of taking one away.
        for right in rights::DENIED {
            assert!(right.starts_with("SeDeny"), "{right} is not a deny right");
        }
    }

    #[test]
    fn reports_start_empty() {
        assert_eq!(ProvisionReport::default().accounts_created.len(), 0);
        assert!(!RemovalReport::default().group_removed);
    }
}
