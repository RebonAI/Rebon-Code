//! Cross-platform child-process containment used by restricted script runners.
//!
//! Unix command setup installs hard resource limits, including `RLIMIT_NOFILE=32`. Windows Job
//! Objects enforce memory, CPU, and active-process limits but expose no numeric quota for general
//! kernel handles. This API therefore does not claim a Windows handle-count boundary: callers must
//! deny guest filesystem, process, module, and import capabilities that could create handles, and
//! use a wall deadline to bound the contained process lifetime.

use std::io;
use std::process::{Child, Command};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Whether a failed `kill(-pgid, SIGKILL)` means the group is already sealed.
///
/// ESRCH is the ordinary answer everywhere: the group has no members left.
/// macOS adds a twist — a zombie cannot receive a signal, and xnu answers
/// EPERM when *no* member of the group can receive one, so a group whose last
/// member is the unreaped leader reports EPERM where Linux delivers to the
/// zombie and discards. Every process in the group runs as this uid (they are
/// our own descendants), so a genuine permission refusal cannot occur and
/// EPERM there is only ever the all-zombies case: a sealed unit.
#[cfg(unix)]
fn group_kill_error_is_sealed(error: &io::Error) -> bool {
    match error.raw_os_error() {
        Some(code) if code == libc::ESRCH => true,
        #[cfg(target_os = "macos")]
        Some(code) if code == libc::EPERM => true,
        _ => false,
    }
}

pub struct Containment {
    #[cfg(windows)]
    job: Option<windows_sys::Win32::Foundation::HANDLE>,
    #[cfg(unix)]
    process_group: i32,
}

/// A deadline-only kill capability. On Windows the duplicated Job handle is a stable kernel
/// identity; on Unix the watchdog state machine guarantees the group leader remains unreaped
/// until this capability has either fired or been disarmed.
pub(crate) struct WatchdogKill {
    #[cfg(windows)]
    job: Option<windows_sys::Win32::Foundation::HANDLE>,
    #[cfg(unix)]
    process_group: i32,
}

impl Containment {
    pub fn configure_command(
        command: &mut Command,
        memory_bytes: u64,
        cpu_seconds: u64,
        active_process_limit: u32,
    ) -> io::Result<()> {
        Self::configure_command_with_stack(
            command,
            memory_bytes,
            cpu_seconds,
            active_process_limit,
            None,
        )
    }

    /// Apply the pre-exec half of the process boundary used by isolated script runners.
    ///
    /// On Unix this installs hard resource limits, including `RLIMIT_NOFILE=32`. Windows command
    /// setup has no equivalent numeric general-handle quota; [`Self::establish`] applies the Job
    /// limits after spawn, and restricted-runtime callers must separately deny handle-creating
    /// guest capabilities.
    ///
    /// `stack_bytes` is enforced with `RLIMIT_STACK` on Unix, except macOS, where xnu refuses
    /// the shrink from a forked pthread worker (EINVAL fails the whole spawn). Windows and macOS
    /// callers must therefore use their runtime's native stack-size option — Windows because Job
    /// Objects expose no stack limit, macOS because the rlimit cannot be installed at all.
    ///
    /// `memory_bytes` is likewise skipped on macOS: xnu refuses to lower `RLIMIT_AS` below the
    /// map's current size, and an Apple Silicon process reserves hundreds of GiB before its
    /// first allocation, so every finite limit is EINVAL. macOS memory containment is the
    /// runtime's own heap bound plus the wall deadline.
    ///
    /// `active_process_limit` of `0` means *no* process bound — no `RLIMIT_NPROC`
    /// on Unix, and no `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` on Windows — for a
    /// runtime that cannot start under one. That limit is counted per **user**, across every
    /// process they already have — so it is not a bound on this child at all, and
    /// any constant is a bet on what else is running. A caller that opts out has to
    /// deny descendants some other way.
    pub fn configure_command_with_stack(
        command: &mut Command,
        memory_bytes: u64,
        cpu_seconds: u64,
        active_process_limit: u32,
        stack_bytes: Option<u64>,
    ) -> io::Result<()> {
        #[cfg(unix)]
        {
            let memory = libc::rlim_t::try_from(memory_bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "memory limit is not representable",
                )
            })?;
            let cpu = libc::rlim_t::try_from(cpu_seconds).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CPU limit is not representable",
                )
            })?;
            let stack = stack_bytes
                .map(libc::rlim_t::try_from)
                .transpose()
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "stack limit is not representable",
                    )
                })?;
            #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
            let processes = libc::rlim_t::from(active_process_limit);
            #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
            let _ = active_process_limit;
            // SAFETY: this closure only calls async-signal-safe libc functions before exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::setpgid(0, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // Not on macOS: xnu refuses to lower RLIMIT_AS below the
                    // map's current size, and on Apple Silicon every process
                    // already spans hundreds of GiB of reservations (shared
                    // cache, pointer-auth regions) before it allocates its
                    // first byte — so any finite limit is EINVAL and kills
                    // the whole spawn. Address-space containment there falls
                    // to the runtime's own heap bound (Node's
                    // `--max-old-space-size`, Boa's engine limits) plus the
                    // wall deadline, the same shape Windows uses Job memory
                    // limits to express.
                    #[cfg(not(target_os = "macos"))]
                    set_limit(libc::RLIMIT_AS, memory, memory)?;
                    #[cfg(target_os = "macos")]
                    let _ = memory;
                    set_limit(libc::RLIMIT_CPU, cpu, cpu)?;
                    set_limit(libc::RLIMIT_NOFILE, 32, 32)?;
                    set_limit(libc::RLIMIT_CORE, 0, 0)?;
                    // Not on macOS: xnu's dosetrlimit refuses to shrink
                    // RLIMIT_STACK out from under a stack it considers in use,
                    // and a fork taken on a pthread worker — which is where
                    // every production spawn happens — answers EINVAL and
                    // kills the whole spawn. Callers already hand the runtime
                    // its native stack option (Node's `--stack_size`), the
                    // same contract Windows relies on for the same reason.
                    #[cfg(not(target_os = "macos"))]
                    if let Some(stack) = stack {
                        set_limit(libc::RLIMIT_STACK, stack, stack)?;
                    }
                    #[cfg(target_os = "macos")]
                    let _ = stack;
                    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "macos"))]
                    if processes != 0 {
                        set_limit(libc::RLIMIT_NPROC, processes, processes)?;
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        let _ = (
            command,
            memory_bytes,
            cpu_seconds,
            active_process_limit,
            stack_bytes,
        );
        Ok(())
    }

    /// Establish the post-spawn containment identity.
    ///
    /// Windows Job Objects enforce memory, CPU, and active-process limits, but not a numeric quota
    /// over general kernel handles. Unix resource limits were already installed before `exec`.
    pub fn establish(
        child: &mut Child,
        memory_bytes: u64,
        cpu_seconds: u64,
        active_process_limit: u32,
    ) -> io::Result<Self> {
        #[cfg(windows)]
        {
            use std::mem::{size_of, zeroed};
            use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
                JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            };
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                PROCESS_TERMINATE,
            };

            let memory_bytes = usize::try_from(memory_bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "memory limit is not representable",
                )
            })?;
            let cpu_100ns = cpu_seconds
                .checked_mul(10_000_000)
                .and_then(|ticks| i64::try_from(ticks).ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "CPU limit is not representable by Windows Job Objects",
                    )
                })?;
            // SAFETY: all pointers refer to initialized structures for the duration of each call.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                // `0` is the caller saying "no process bound", and on this side
                // that has to mean *drop the flag*: a Job whose
                // `ActiveProcessLimit` is zero admits no processes at all, so
                // assigning the child to it fails with ERROR_NOT_ENOUGH_QUOTA
                // and the run never starts. The Unix side reads the same zero as
                // "skip the rlimit"; only here does it invert.
                let bound_processes = active_process_limit != 0;
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                    | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                    | JOB_OBJECT_LIMIT_JOB_MEMORY
                    | JOB_OBJECT_LIMIT_JOB_TIME
                    | if bound_processes {
                        JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                    } else {
                        0
                    };
                info.BasicLimitInformation.PerJobUserTimeLimit = cpu_100ns;
                info.BasicLimitInformation.ActiveProcessLimit = active_process_limit;
                info.ProcessMemoryLimit = memory_bytes;
                info.JobMemoryLimit = memory_bytes;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == FALSE
                {
                    let error = io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error);
                }
                let process = OpenProcess(
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    FALSE,
                    child.id(),
                );
                if process == 0 {
                    let error = io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error);
                }
                let assigned = AssignProcessToJobObject(job, process);
                CloseHandle(process);
                if assigned == FALSE {
                    let error = io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error);
                }
                Ok(Self { job: Some(job) })
            }
        }
        #[cfg(unix)]
        {
            let _ = (memory_bytes, cpu_seconds, active_process_limit);
            let process_group = i32::try_from(child.id()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "child PID is not representable as a Unix process group",
                )
            })?;
            Ok(Self { process_group })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (child, memory_bytes, cpu_seconds, active_process_limit);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process containment is unsupported on this platform",
            ))
        }
    }

    pub(crate) fn watchdog_kill(&self) -> io::Result<WatchdogKill> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
            use windows_sys::Win32::Foundation::{DuplicateHandle, FALSE, HANDLE};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;

            let job = self.job.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "containment is already sealed")
            })?;
            let process = unsafe { GetCurrentProcess() };
            let mut duplicate: HANDLE = 0;
            // SAFETY: job is live while borrowed from self; duplicate is a valid out pointer.
            if unsafe {
                DuplicateHandle(
                    process,
                    job,
                    process,
                    &mut duplicate,
                    0,
                    FALSE,
                    DUPLICATE_SAME_ACCESS,
                )
            } == FALSE
            {
                return Err(io::Error::last_os_error());
            }
            Ok(WatchdogKill {
                job: Some(duplicate),
            })
        }
        #[cfg(unix)]
        {
            Ok(WatchdogKill {
                process_group: self.process_group,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported containment watchdog",
            ))
        }
    }

    /// Terminates descendants and, on Windows, consumes the Job handle. Closing a
    /// KILL_ON_JOB_CLOSE Job is an independent backstop if explicit termination fails.
    pub fn terminate_and_seal(&mut self, direct_child_exited: bool) -> io::Result<()> {
        #[cfg(windows)]
        {
            use std::mem::{size_of, zeroed};
            use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, FALSE};
            use windows_sys::Win32::System::JobObjects::{
                JobObjectBasicAccountingInformation, QueryInformationJobObject, TerminateJobObject,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            };

            let Some(job) = self.job.take() else {
                return Ok(());
            };
            let mut termination_error = None;
            // SAFETY: job was owned by self and remains valid until CloseHandle below.
            if unsafe { TerminateJobObject(job, 137) } == FALSE {
                let error = io::Error::last_os_error();
                let benign = if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
                    // SAFETY: info is writable and has the exact queried structure size.
                    let queried = unsafe {
                        QueryInformationJobObject(
                            job,
                            JobObjectBasicAccountingInformation,
                            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                            std::ptr::null_mut(),
                        )
                    } != FALSE;
                    (queried && info.ActiveProcesses == 0) || direct_child_exited
                } else {
                    false
                };
                if !benign {
                    termination_error = Some(error);
                }
            }
            // SAFETY: take() guarantees this owned handle is closed exactly once.
            let close_error = if unsafe { CloseHandle(job) } == FALSE {
                Some(io::Error::last_os_error())
            } else {
                None
            };
            termination_error.or(close_error).map_or(Ok(()), Err)
        }
        #[cfg(unix)]
        {
            let _ = direct_child_exited;
            // SAFETY: a negative pid targets the process group established before exec. The
            // direct leader is still owned and unreaped here; ESRCH therefore means the group is
            // already empty, which is a successfully sealed containment unit on every outcome.
            if unsafe { libc::kill(-self.process_group, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if !group_kill_error_is_sealed(&error) {
                    return Err(error);
                }
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = direct_child_exited;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported containment",
            ))
        }
    }
}

impl WatchdogKill {
    pub(crate) fn terminate(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            use std::mem::{size_of, zeroed};
            use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, FALSE};
            use windows_sys::Win32::System::JobObjects::{
                JobObjectBasicAccountingInformation, QueryInformationJobObject, TerminateJobObject,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            };

            let job = self.job.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "watchdog handle is already closed",
                )
            })?;
            // SAFETY: the duplicated Job handle remains owned by self during these calls.
            if unsafe { TerminateJobObject(job, 137) } != FALSE {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
                return Err(error);
            }
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            // SAFETY: info is writable and has the exact queried structure size.
            let queried = unsafe {
                QueryInformationJobObject(
                    job,
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            } != FALSE;
            if queried && info.ActiveProcesses == 0 {
                Ok(())
            } else {
                Err(error)
            }
        }
        #[cfg(unix)]
        {
            // SAFETY: the parent keeps the direct child unreaped while this watchdog is armed, so
            // the process-group number cannot be reused by an unrelated later process.
            if unsafe { libc::kill(-self.process_group, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if !group_kill_error_is_sealed(&error) {
                    return Err(error);
                }
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported containment watchdog",
            ))
        }
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
type RlimitResource = libc::c_int;

#[cfg(unix)]
unsafe fn set_limit(
    resource: RlimitResource,
    soft: libc::rlim_t,
    hard: libc::rlim_t,
) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    // SAFETY: limit points to a valid rlimit and resource is a platform constant.
    if unsafe { libc::setrlimit(resource, &limit) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WatchdogKill {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            // SAFETY: the duplicated handle is owned by this value and closed exactly once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
        }
    }
}

#[cfg(windows)]
impl Drop for Containment {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            // Panic-only backstop; normal paths explicitly seal before returning.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
        }
    }
}
