//! Whether this process can do the privileged half.
//!
//! `install` and `uninstall` are the only two subcommands that need
//! administrator rights, and the check is here rather than left to the first
//! Win32 call that fails. A partially-applied install is the worst state this
//! system has: some accounts created, some not, and a `status` that reports
//! neither "installed" nor "clean". Refusing before the first change is what
//! keeps that from happening by accident.
//!
//! The message matters as much as the check. "Access denied" from `NetUserAdd`
//! tells a user nothing; a sentence naming the command and how to run it
//! elevated is the difference between a fixable problem and a bug report.

use crate::sys::SysResult;

/// Whether this process holds an elevated token.
pub fn is_elevated() -> bool {
    imp::is_elevated()
}

/// Refuse unless elevated.
pub fn require_elevation(command: &str) -> SysResult<()> {
    if is_elevated() {
        return Ok(());
    }
    Err(crate::sys::SysError::Invalid(format!(
        "`sandbox-win.exe {command}` needs administrator rights. Open a terminal with \
         \"Run as administrator\" and run it there, or use `sudo sandbox-win.exe {command}` \
         if you have one. Running a sandboxed command does NOT need elevation — only \
         install and uninstall do."
    )))
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub fn is_elevated() -> bool {
        // `TokenElevation` rather than "is the user in Administrators": under UAC a
        // split-token admin is in the group and still cannot create an account until
        // the elevated token is the one running. The group check answers a different
        // question and answers it wrong.
        let mut token: HANDLE = 0;
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as *mut c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        };
        unsafe { CloseHandle(token) };
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_elevated() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_names_the_command_and_a_way_to_run_it() {
        let error = require_elevation("install").unwrap_err().to_string();
        if is_elevated() {
            return;
        }
        assert!(error.contains("sandbox-win.exe install"), "{error}");
        assert!(error.contains("Run as administrator"), "{error}");
    }

    #[test]
    fn the_refusal_says_running_a_command_does_not_need_elevation() {
        // The single most important fact about this binary, and the place a user is most
        // likely to conclude the opposite.
        if is_elevated() {
            return;
        }
        let error = require_elevation("install").unwrap_err().to_string();
        assert!(error.contains("does NOT need elevation"), "{error}");
    }

    #[test]
    fn an_elevated_process_is_not_refused() {
        if !is_elevated() {
            return;
        }
        assert!(require_elevation("install").is_ok());
    }

    #[test]
    fn the_check_does_not_panic_on_any_platform() {
        let _ = is_elevated();
    }
}
