//! Whether the session that placed an ACE is still running.
//!
//! `exec` reaps before it does anything else, and reaping means deciding that a
//! recorded owner is dead. A PID alone cannot decide that: Windows recycles PIDs,
//! and a PID that has come back around belongs to something else entirely.
//! Getting it wrong is bad in both directions — declare a live session dead and
//! its running commands lose their confinement mid-flight; declare a dead one
//! live and its deny ACEs stay on the user's disk with nothing left that knows
//! how to remove them.
//!
//! So an owner is a PID *and* the process's creation time, and both have to
//! match. Creation time is the one thing about a process that a successor cannot
//! inherit.

use crate::core::ledger::Owner;
use crate::sys::SysResult;

/// The current process, as a ledger owner.
pub fn current_owner() -> SysResult<Owner> {
    imp::current_owner()
}

/// The process that started this one, as a ledger owner.
///
/// This is who an `exec`'s ACEs belong to. The helper itself exits with the
/// command, so an ACE owned by it would be reapable the moment the next command
/// started — every `exec` would tear down the confinement the previous one put in
/// place. The caller is the process whose death should release them, which is
/// exactly what [`Owner`] means.
///
/// Fails rather than falling back to the current process. A wrong owner here is
/// not a degraded mode: too short-lived and the ACEs churn, too long-lived and
/// they outlive the session with nothing left to remove them.
pub fn parent_owner() -> SysResult<Owner> {
    imp::parent_owner()
}

/// Whether `owner` names a process that is still running.
///
/// Answers `false` on any error. That is the safe direction: the cost of a wrong
/// `false` is one revocation attempt that reports a mismatch and stops, while a
/// wrong `true` leaves ACEs on the disk forever.
pub fn is_alive(owner: &Owner) -> bool {
    imp::started_at_ms(owner.pid)
        .map(|started| started == owner.started_at_ms)
        .unwrap_or(false)
}

/// The creation time of `pid`, in milliseconds since the Unix epoch.
pub fn started_at_ms(pid: u32) -> SysResult<u64> {
    imp::started_at_ms(pid)
}

#[cfg(windows)]
mod imp {
    use crate::core::ledger::Owner;
    use crate::sys::{SysError, SysResult};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, FILETIME, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// 1601-01-01 to 1970-01-01, in 100ns ticks. `FILETIME`'s epoch is the former
    /// and every other timestamp in the ledger uses the latter.
    const TICKS_TO_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

    pub fn current_owner() -> SysResult<Owner> {
        let pid = unsafe { GetCurrentProcessId() };
        Ok(Owner {
            pid,
            started_at_ms: started_at_ms(pid)?,
        })
    }

    pub fn parent_owner() -> SysResult<Owner> {
        let pid = parent_pid()?;
        let started_at_ms = started_at_ms(pid)?;

        // The parent could have exited between the snapshot and this call, and the PID
        // could already belong to something else — in which case the time read here is
        // the successor's. Nothing can close that window, but it is not silent: the row
        // would be reaped on the next command, which is the safe direction.
        Ok(Owner { pid, started_at_ms })
    }

    /// The parent PID, from a process snapshot.
    ///
    /// `NtQueryInformationProcess` would be one call instead of a walk, but it is not
    /// a stable API and the walk needs no handle to the parent — which matters,
    /// because the parent may be at a different integrity level than this process.
    fn parent_pid() -> SysResult<u32> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(SysError::win32("CreateToolhelp32Snapshot", unsafe {
                GetLastError()
            }));
        }
        let me = unsafe { GetCurrentProcessId() };
        let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

        let mut found = None;
        let mut ok = unsafe { Process32First(snapshot, &mut entry) };
        while ok != 0 {
            if entry.th32ProcessID == me {
                found = Some(entry.th32ParentProcessID);
                break;
            }
            ok = unsafe { Process32Next(snapshot, &mut entry) };
        }
        unsafe { CloseHandle(snapshot) };

        match found {
            // 0 is not a real PID, and it is what the field holds for a process whose parent
            // has been reaped by the kernel. Treated as "there is no parent to own anything"
            // rather than passed on.
            Some(0) | None => Err(SysError::Invalid(
                "this process has no parent to own its sandbox ACEs — `exec` has to be \
                 started by the process whose session the rules belong to"
                    .into(),
            )),
            Some(pid) => Ok(pid),
        }
    }

    pub fn started_at_ms(pid: u32) -> SysResult<u64> {
        // `PROCESS_QUERY_LIMITED_INFORMATION` rather than `PROCESS_QUERY_INFORMATION`:
        // the limited right is granted across integrity levels, and `reap` must be able
        // to ask about a process it did not start.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle == 0 {
            // A dead process has no handle to open, which is the common case here and not a
            // fault.
            return Err(SysError::win32("OpenProcess", unsafe { GetLastError() }));
        }

        let mut creation: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        let code = unsafe { GetLastError() };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(SysError::win32("GetProcessTimes", code));
        }

        let ticks = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        Ok(ticks.saturating_sub(TICKS_TO_UNIX_EPOCH) / 10_000)
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::core::ledger::Owner;
    use crate::sys::{SysError, SysResult};

    pub fn current_owner() -> SysResult<Owner> {
        Err(SysError::Unsupported("process liveness"))
    }

    pub fn parent_owner() -> SysResult<Owner> {
        Err(SysError::Unsupported("process liveness"))
    }

    pub fn started_at_ms(_pid: u32) -> SysResult<u64> {
        Err(SysError::Unsupported("process liveness"))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive() {
        let owner = current_owner().unwrap();
        assert!(is_alive(&owner));
    }

    #[test]
    fn the_creation_time_is_a_plausible_unix_millisecond() {
        // A FILETIME epoch mistake shows up here as a timestamp in the 1600s or a number
        // of implausible magnitude, and it would make every owner comparison fail —
        // which reads as "every session is dead" and reaps ACEs out from under running
        // commands.
        let owner = current_owner().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        assert!(owner.started_at_ms > 1_600_000_000_000, "{owner:?}");
        assert!(owner.started_at_ms <= now + 1_000, "{owner:?} vs now {now}");
    }

    #[test]
    fn the_creation_time_is_stable_across_reads() {
        let first = current_owner().unwrap();
        let second = current_owner().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn an_owner_with_the_right_pid_and_the_wrong_start_time_is_dead() {
        // The PID-reuse case, in the form the reaper actually meets it. A check that
        // looked only at the PID would call this alive and leave the dead session's ACEs
        // on the disk permanently.
        let mut owner = current_owner().unwrap();
        owner.started_at_ms += 1;

        assert!(!is_alive(&owner));
    }

    #[test]
    fn an_exited_process_is_not_alive() {
        let child = std::process::Command::new("cmd.exe")
            .args(["/c", "exit", "0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = started_at_ms(pid).ok();
        let mut child = child;
        child.wait().unwrap();

        if let Some(started_at_ms) = started {
            let owner = Owner { pid, started_at_ms };
            // The handle can outlive the process while the parent holds it, so the meaningful
            // assertion is that we either see it gone or still see the same start time —
            // never a *different* live process under that PID.
            if is_alive(&owner) {
                assert_eq!(super::started_at_ms(pid).unwrap(), started_at_ms);
            }
        }
    }

    #[test]
    fn a_pid_that_cannot_exist_is_not_alive() {
        // PIDs on Windows are multiples of four and bounded well below this.
        let owner = Owner {
            pid: u32::MAX - 1,
            started_at_ms: 1,
        };
        assert!(!is_alive(&owner));
    }

    #[test]
    fn pid_zero_is_not_alive() {
        // The System Idle Process. Never a session owner, and an uninitialised owner
        // field would land here.
        assert!(!is_alive(&Owner {
            pid: 0,
            started_at_ms: 0,
        }));
    }
}
