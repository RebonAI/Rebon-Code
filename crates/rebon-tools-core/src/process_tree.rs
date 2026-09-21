//! One guard over a spawned child and everything it spawns.
//!
//! Killing a child is not killing what the child started. A shell runs its
//! command in a subprocess, `npx` execs node, `rustup` proxies the real
//! toolchain binary — signal the direct child and the grandchildren survive,
//! holding pipes open and, on Windows, files locked. Both platforms have an
//! answer, and they are not the same shape:
//!
//! * Windows: the child joins a job object created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the kernel reaps the whole tree
//!   when the last handle to the job closes — including when this process is
//!   terminated without running destructors. Job membership is inherited, so
//!   descendants are covered without any spawn-time setup.
//! * Unix: the child is spawned as its own process-group leader (the caller
//!   arranges that with `process_group(0)`), and the group is signalled as a
//!   whole with a negative pid.
//!
//! The caller passes the raw handle or pid rather than a `Child`, so that this
//! crate stays at the bottom of the dependency graph and does not care whether
//! the child came from `std` or from an async runtime.
//!
//! # Terminating twice
//!
//! [`ProcessTreeGuard::terminate`] is safe to call before the guard is
//! dropped, which is the common shape: shut down deliberately, then let `Drop`
//! be the backstop for every path that did not. On unix that repeat matters —
//! a process-group id is a small recycled integer, and signalling one twice
//! risks the second signal landing on whatever inherited the number. So the
//! unix guard disarms itself on the first terminate and every later call, the
//! one in `Drop` included, does nothing. Windows needs no such care: the guard
//! owns the job handle for its whole life, so a second `TerminateJobObject`
//! cannot reach a stranger.

use std::io;

/// A spawned process and its descendants, terminated together.
///
/// Dropping the guard terminates the tree. See the [module
/// docs](self) for what that means on each platform.
#[cfg(windows)]
#[derive(Debug)]
pub struct ProcessTreeGuard {
    job: windows_sys::Win32::Foundation::HANDLE,
}

/// A spawned process and its descendants, terminated together.
///
/// Dropping the guard terminates the tree. See the [module
/// docs](self) for what that means on each platform.
#[cfg(unix)]
#[derive(Debug)]
pub struct ProcessTreeGuard {
    /// `None` once the group has been signalled. See "Terminating twice" in
    /// the module docs for why this is not simply an `i32`.
    process_group_id: Option<i32>,
}

#[cfg(windows)]
impl ProcessTreeGuard {
    /// Put an already-spawned process, and everything it goes on to spawn,
    /// into a fresh job object.
    ///
    /// `handle` is the child's process handle, as
    /// `std::os::windows::io::AsRawHandle::as_raw_handle` or an async
    /// runtime's equivalent returns it. The handle is only read here; the
    /// caller keeps ownership of it, and of waiting on the child.
    ///
    /// `breakaway` sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, which exempts
    /// nothing on its own: descendants still join the job unless one
    /// explicitly asks to break away. Pass it only when a child is expected
    /// to launch something that must outlive this tree — a shared daemon,
    /// say — and pass `false` otherwise, so that nothing can opt out of
    /// being reaped.
    ///
    /// The job handle is closed again on every failure path, so a failed
    /// call leaks nothing.
    pub fn for_raw_handle(
        handle: std::os::windows::io::RawHandle,
        breakaway: bool,
    ) -> io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = if breakaway {
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK
        } else {
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        };
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = io::Error::last_os_error();
            close_job(job);
            return Err(error);
        }

        let process = handle as windows_sys::Win32::Foundation::HANDLE;
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            let error = io::Error::last_os_error();
            close_job(job);
            return Err(error);
        }

        Ok(Self { job })
    }

    /// Whether a [`terminate`](Self::terminate) would still signal anything.
    ///
    /// Always true on Windows: the guard holds the job handle until it is
    /// dropped, and terminating a job twice is harmless.
    pub fn is_armed(&self) -> bool {
        true
    }

    /// Kill the whole tree now, rather than waiting for the drop.
    pub fn terminate(&mut self) -> io::Result<()> {
        let terminated =
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
fn close_job(job: windows_sys::Win32::Foundation::HANDLE) {
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(job);
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE reaps the tree as the last handle closes, so
        // this one call is both the release and the kill.
        close_job(self.job);
    }
}

#[cfg(unix)]
impl ProcessTreeGuard {
    /// Guard the process group led by `pid`.
    ///
    /// The caller is responsible for having spawned the child as its own
    /// group leader — `std::os::unix::process::CommandExt::process_group(0)`
    /// — so that `pid` is also the group id. Signalling a pid that does not
    /// lead a group would reach that one process and none of its children.
    pub fn for_process_group(pid: u32) -> io::Result<Self> {
        let process_group_id =
            i32::try_from(pid).map_err(|_| io::Error::other("child pid exceeds i32"))?;
        Ok(Self {
            process_group_id: Some(process_group_id),
        })
    }

    /// Whether a [`terminate`](Self::terminate) would still signal anything.
    ///
    /// False once the group has been signalled once.
    pub fn is_armed(&self) -> bool {
        self.process_group_id.is_some()
    }

    /// Kill the whole group now, rather than waiting for the drop.
    ///
    /// A group that is already gone is success, not failure: `ESRCH` only
    /// says the tree died before this call reached it, which is the outcome
    /// being asked for. The guard disarms whether or not the signal landed,
    /// so this never signals the same group id twice.
    pub fn terminate(&mut self) -> io::Result<()> {
        let Some(process_group_id) = self.process_group_id.take() else {
            return Ok(());
        };
        if unsafe { libc::kill(-process_group_id, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn spawn_group_leader() -> std::process::Child {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        command.spawn().expect("spawn group leader")
    }

    /// `kill(0)` probes for the group without signalling it.
    fn group_exists(process_group_id: i32) -> bool {
        unsafe { libc::kill(-process_group_id, 0) == 0 }
    }

    fn wait_until_group_is_gone(process_group_id: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !group_exists(process_group_id) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn terminating_kills_the_whole_group() {
        let mut child = spawn_group_leader();
        let process_group_id = i32::try_from(child.id()).unwrap();
        let mut guard = ProcessTreeGuard::for_process_group(child.id()).unwrap();
        assert!(group_exists(process_group_id));

        guard.terminate().expect("terminate the group");

        let _ = child.wait();
        assert!(wait_until_group_is_gone(process_group_id));
    }

    /// The point of the `Option`: a group id is a recycled integer, so the
    /// second signal — the one `Drop` would send after a deliberate
    /// shutdown — must never leave this process.
    #[test]
    fn a_second_terminate_does_not_signal_again() {
        let mut child = spawn_group_leader();
        let mut guard = ProcessTreeGuard::for_process_group(child.id()).unwrap();
        assert!(guard.is_armed());

        guard.terminate().expect("first terminate");
        assert!(
            !guard.is_armed(),
            "the guard should disarm on the first kill"
        );

        guard.terminate().expect("second terminate is a no-op");
        assert!(!guard.is_armed());
        drop(guard);

        let _ = child.wait();
    }

    /// A tree that died on its own is the outcome `terminate` wants, so it
    /// is not an error.
    ///
    /// The group id here is deliberately above any pid the kernel can hand
    /// out — `pid_max` tops out at 2^22 — so this asks about a group that
    /// cannot exist rather than re-signalling a reaped one, whose id could
    /// by then belong to somebody else.
    #[test]
    fn terminating_a_group_that_is_already_gone_succeeds() {
        let mut guard = ProcessTreeGuard::for_process_group(i32::MAX as u32).unwrap();

        guard
            .terminate()
            .expect("a group that is already gone is success");
    }

    #[test]
    fn a_pid_that_does_not_fit_an_i32_is_rejected() {
        let error = ProcessTreeGuard::for_process_group(u32::MAX).unwrap_err();
        assert!(error.to_string().contains("exceeds i32"), "{error}");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::process::{Command, Stdio};

    fn spawn_child() -> std::process::Child {
        Command::new("cmd")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child")
    }

    #[test]
    fn terminating_kills_the_child() {
        let mut child = spawn_child();
        let mut guard = ProcessTreeGuard::for_raw_handle(child.as_raw_handle(), false)
            .expect("guard the child");
        assert!(child.try_wait().unwrap().is_none());

        guard.terminate().expect("terminate the job");

        let status = child.wait().expect("wait for the killed child");
        assert!(!status.success());
    }

    /// Closing the last job handle reaps the tree with exit code 0, so what
    /// says the guard did it is that the wait returns long before the child
    /// would have finished on its own.
    #[test]
    fn dropping_the_guard_kills_the_child() {
        let mut child = spawn_child();
        let guard = ProcessTreeGuard::for_raw_handle(child.as_raw_handle(), false)
            .expect("guard the child");

        let started = std::time::Instant::now();
        drop(guard);
        child.wait().expect("wait for the reaped child");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the child outlived its guard"
        );
    }

    #[test]
    fn breakaway_is_accepted_as_a_job_limit() {
        let mut child = spawn_child();
        let mut guard = ProcessTreeGuard::for_raw_handle(child.as_raw_handle(), true)
            .expect("a breakaway job is a valid job");

        guard.terminate().expect("terminate the job");

        let _ = child.wait();
    }

    /// The job handle has to be closed on the failure path too, which is
    /// only observable as the call itself failing rather than hanging or
    /// leaking.
    #[test]
    fn an_invalid_handle_is_an_error() {
        let error = ProcessTreeGuard::for_raw_handle(std::ptr::null_mut(), false)
            .expect_err("a null process handle cannot be assigned to a job");
        assert!(error.raw_os_error().is_some(), "{error}");
    }
}
