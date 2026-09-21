//! Windows integrity-level checks (UIPI).
//!
//! A non-elevated process cannot inject input into an elevated one; Windows
//! silently drops the events. Detect the mismatch up front and refuse with a
//! clear error instead of pretending the action happened.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::runtime::{ComputerUseError, ErrorCode};

/// Refuses targets running at a higher integrity level than Rebon itself.
pub(super) fn ensure_target_integrity(pid: u32) -> Result<(), ComputerUseError> {
    let own_level = process_integrity_level(unsafe { GetCurrentProcess() })
        .ok_or_else(|| integrity_error("could not determine Rebon's integrity level"))?;
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(integrity_error(
            "the target process cannot be inspected; it may run as administrator",
        ));
    }
    let target_level = process_integrity_level(process);
    unsafe { CloseHandle(process) };
    let target_level = target_level.ok_or_else(|| {
        integrity_error("the target process integrity level cannot be determined")
    })?;
    if target_level > own_level {
        return Err(integrity_error(
            "the target application runs at a higher integrity level (as administrator); Computer Use cannot control it",
        ));
    }
    Ok(())
}

fn process_integrity_level(process: HANDLE) -> Option<u32> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let level = token_integrity_level(token);
    unsafe { CloseHandle(token) };
    level
}

fn token_integrity_level(token: HANDLE) -> Option<u32> {
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return None;
    }
    let mut buffer = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    let label = unsafe { &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let sid = label.Label.Sid;
    if sid.is_null() {
        return None;
    }
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

fn integrity_error(message: &str) -> ComputerUseError {
    ComputerUseError::new(ErrorCode::PermissionDenied, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_integrity_level_is_readable_and_self_control_is_allowed() {
        let level = process_integrity_level(unsafe { GetCurrentProcess() });
        assert!(level.is_some());
        // A process is never at a higher level than itself.
        ensure_target_integrity(std::process::id()).unwrap();
    }
}
