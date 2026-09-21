#[cfg(any(windows, target_os = "macos"))]
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(unix)]
use anyhow::Context;

pub fn wait_for_pid_exit(pid: u32, timeout: Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if process_is_running(pid) == Some(false) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!(
        "process {pid} did not exit within {} ms",
        timeout.as_millis()
    )
}

pub fn recorded_process_is_running(
    pid: u32,
    expected_identity: Option<&str>,
) -> anyhow::Result<bool> {
    match process_is_running(pid) {
        Some(false) => return Ok(false),
        Some(true) => {}
        None => anyhow::bail!("could not determine whether process {pid} is running"),
    }
    let expected_identity = expected_identity
        .ok_or_else(|| anyhow::anyhow!("process {pid} has no recorded identity"))?;
    let Some(actual_identity) = process_identity(pid) else {
        anyhow::bail!("could not verify the identity of process {pid}");
    };
    Ok(actual_identity == expected_identity)
}

pub fn wait_for_recorded_process_exit(
    pid: u32,
    expected_identity: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        if !recorded_process_is_running(pid, expected_identity)? {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "process {pid} did not exit within {} ms",
                timeout.as_millis()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Terminate a process whose recorded identity we verified.
///
/// The identity is the provenance: only a background process this codebase
/// spawned carries one, and those are spawned detached into a session of
/// their own. That is what licenses signalling the whole process group here
/// — the children a worker started without claiming a group go with it,
/// instead of surviving as orphans holding the API key and the workspace.
pub fn terminate_recorded_process(
    pid: u32,
    expected_identity: Option<&str>,
    detached_group: bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    terminate_recorded_process_tree(pid, expected_identity, detached_group, timeout)
}

/// Terminate exactly the recorded process owner and, when durable spawn
/// provenance proves it leads a dedicated Unix group, all non-zombie members
/// of that group. A reused live PID with a different identity is never
/// group-signalled.
pub fn recorded_process_tree_is_running(
    pid: u32,
    expected_identity: Option<&str>,
    owner_detached_group: bool,
) -> anyhow::Result<bool> {
    #[cfg(unix)]
    if owner_detached_group {
        verify_group_leader_identity(pid, expected_identity)?;
        return Ok(!active_unix_group_members(pid)?.is_empty());
    }
    #[cfg(not(unix))]
    let _ = owner_detached_group;

    recorded_process_is_running(pid, expected_identity)
}

pub fn terminate_recorded_process_tree(
    pid: u32,
    expected_identity: Option<&str>,
    owner_detached_group: bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    if owner_detached_group {
        return terminate_recorded_unix_group(pid, expected_identity, timeout);
    }
    #[cfg(not(unix))]
    let _ = owner_detached_group;

    if !recorded_process_is_running(pid, expected_identity)? {
        return Ok(());
    }
    if terminate_process(pid).is_none() && recorded_process_is_running(pid, expected_identity)? {
        anyhow::bail!("failed to terminate recorded process {pid}");
    }
    wait_for_recorded_process_exit(pid, expected_identity, timeout)
}

/// Whether `pid` is currently the leader of its own process group. This is
/// observation only; callers still need trusted spawn provenance before
/// persisting `owner_detached_group`.
#[cfg(unix)]
pub fn process_owns_detached_group(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    unsafe { libc::getpgid(pid) == pid }
}

#[cfg(not(unix))]
pub fn process_owns_detached_group(_pid: u32) -> bool {
    false
}

/// Terminate a process by pid alone, without identity verification. Only for
/// legacy job states that predate `pid_identity`: refusing to act on them
/// would leave their jobs permanently unstoppable, which is worse than the
/// residual pid-reuse risk this accepts.
///
/// Single pid, never a process group: with no recorded identity there is no
/// evidence this pid is even ours, and a group signal would then reach
/// processes nobody here started.
pub fn terminate_process_best_effort(pid: u32, timeout: Duration) -> anyhow::Result<()> {
    if process_is_running(pid) == Some(false) {
        return Ok(());
    }
    if terminate_process(pid).is_none() && process_is_running(pid) != Some(false) {
        anyhow::bail!("failed to terminate process {pid}");
    }
    wait_for_pid_exit(pid, timeout)
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnixGroupMember {
    pid: u32,
    state: char,
}

#[cfg(unix)]
fn checked_unix_pid(pid: u32) -> anyhow::Result<libc::pid_t> {
    libc::pid_t::try_from(pid).map_err(|_| anyhow::anyhow!("process id {pid} exceeds pid_t"))
}

#[cfg(unix)]
fn classify_unix_kill_error(target: libc::pid_t, error: std::io::Error) -> anyhow::Result<bool> {
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        // EPERM proves the process/group exists. It is not an absence result;
        // propagate it so Stop and the updater fail closed.
        Some(libc::EPERM) => Err(anyhow::anyhow!(
            "process target {target} exists but cannot be signalled: {error}"
        )),
        _ => Err(error.into()),
    }
}

#[cfg(unix)]
fn unix_kill(target: libc::pid_t, signal: libc::c_int) -> anyhow::Result<bool> {
    if unsafe { libc::kill(target, signal) } == 0 {
        return Ok(true);
    }
    classify_unix_kill_error(target, std::io::Error::last_os_error())
}

#[cfg(unix)]
fn verify_group_leader_identity(pid: u32, expected_identity: Option<&str>) -> anyhow::Result<bool> {
    let checked_pid = checked_unix_pid(pid)?;
    match unix_kill(checked_pid, 0) {
        Ok(false) => Ok(false),
        Ok(true) => {
            // A zombie is no longer runnable, but its PID still exists and cannot
            // have been reused until it is reaped. Identity-check it here even
            // though ordinary liveness treats it as exited; otherwise a stale
            // record could authorize signalling an unrelated group.
            let expected = expected_identity
                .ok_or_else(|| anyhow::anyhow!("process {pid} has no recorded identity"))?;
            let actual = process_identity(pid)
                .ok_or_else(|| anyhow::anyhow!("could not verify the identity of process {pid}"))?;
            if actual != expected {
                anyhow::bail!(
                    "refusing to signal process group {pid}: present leader identity does not match the recorded owner"
                );
            }
            Ok(true)
        }
        Err(error) => {
            Err(error).with_context(|| format!("could not safely probe process-group leader {pid}"))
        }
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_proc_stat(stat: &str) -> anyhow::Result<(char, u32)> {
    let command_end = stat
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("missing process command terminator"))?;
    let mut fields = stat[command_end + 1..].split_whitespace();
    let state = fields
        .next()
        .and_then(|field| field.chars().next())
        .ok_or_else(|| anyhow::anyhow!("missing process state"))?;
    let _parent_pid = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing parent process id"))?;
    let process_group = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing process group id"))?
        .parse::<u32>()
        .context("invalid process group id")?;
    Ok((state, process_group))
}

#[cfg(target_os = "linux")]
fn unix_group_members(process_group: u32) -> anyhow::Result<Vec<UnixGroupMember>> {
    let mut members = Vec::new();
    for entry in std::fs::read_dir("/proc").context("failed to enumerate /proc")? {
        let entry = entry.context("failed to enumerate /proc entry")?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path().join("stat");
        let stat = match std::fs::read_to_string(&path) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound || !entry.path().exists() =>
            {
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read process state for candidate child {pid}")
                });
            }
        };
        let (state, observed_group) = parse_linux_proc_stat(&stat)
            .with_context(|| format!("failed to parse process state for candidate child {pid}"))?;
        if observed_group == process_group {
            members.push(UnixGroupMember { pid, state });
        }
    }
    members.sort_unstable_by_key(|member| member.pid);
    Ok(members)
}

#[cfg(target_os = "macos")]
fn unix_group_members(process_group: u32) -> anyhow::Result<Vec<UnixGroupMember>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,pgid=,state="])
        .stdin(Stdio::null())
        .output()
        .context("failed to enumerate process groups with ps")?;
    if !output.status.success() {
        anyhow::bail!(
            "ps failed while enumerating process groups: {}",
            output.status
        );
    }
    let stdout = String::from_utf8(output.stdout).context("ps returned non-UTF-8 process data")?;
    let mut members = Vec::new();
    for (line_index, line) in stdout.lines().enumerate() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 3 {
            anyhow::bail!(
                "ps returned malformed process data on line {}",
                line_index + 1
            );
        }
        let pid = fields[0]
            .parse::<u32>()
            .with_context(|| format!("invalid pid from ps on line {}", line_index + 1))?;
        let pgid = fields[1]
            .parse::<u32>()
            .with_context(|| format!("invalid pgid from ps on line {}", line_index + 1))?;
        if pgid == process_group {
            let state = fields[2].chars().next().unwrap_or('?');
            members.push(UnixGroupMember { pid, state });
        }
    }
    members.sort_unstable_by_key(|member| member.pid);
    Ok(members)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn unix_group_members(_process_group: u32) -> anyhow::Result<Vec<UnixGroupMember>> {
    anyhow::bail!("process-group membership enumeration is unsupported on this Unix target")
}

#[cfg(unix)]
fn active_members(members: Vec<UnixGroupMember>) -> Vec<u32> {
    members
        .into_iter()
        // Linux and macOS retain zombies until their parent reaps them. They
        // cannot run or spawn and must not keep Stop wedged forever.
        .filter(|member| member.state != 'Z')
        .map(|member| member.pid)
        .collect()
}

#[cfg(unix)]
fn active_unix_group_members(process_group: u32) -> anyhow::Result<Vec<u32>> {
    Ok(active_members(unix_group_members(process_group)?))
}

#[cfg(unix)]
fn wait_for_recorded_unix_group_exit(
    pid: u32,
    expected_identity: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        if process_is_running(pid) == Some(true) {
            verify_group_leader_identity(pid, expected_identity)?;
        }
        let active = active_unix_group_members(pid)?;
        if active.is_empty() {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "process group {pid} still has active members {:?} after {} ms",
                active,
                timeout.as_millis()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
fn terminate_recorded_unix_group(
    pid: u32,
    expected_identity: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let _leader_alive = verify_group_leader_identity(pid, expected_identity)?;
    let active_before = active_unix_group_members(pid)?;
    if active_before.is_empty() {
        return Ok(());
    }
    // Recheck after enumeration. The scan can take long enough for the leader
    // to exit or for a stale pid to become visibly mismatched; never turn that
    // observation into a signal against the newly observed process group.
    let _leader_alive = verify_group_leader_identity(pid, expected_identity)?;
    let group = checked_unix_pid(pid)?
        .checked_neg()
        .ok_or_else(|| anyhow::anyhow!("cannot represent process group {pid}"))?;
    if !unix_kill(group, libc::SIGTERM)? {
        // ESRCH is only success if an honest postcondition scan agrees.
        let remaining = active_unix_group_members(pid)?;
        if remaining.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "process group {pid} disappeared from kill(2) but still has active members {:?}",
            remaining
        );
    }
    wait_for_recorded_unix_group_exit(pid, expected_identity, timeout)
}

#[cfg(windows)]
fn hide_command_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(windows)]
pub fn terminate_process(pid: u32) -> Option<()> {
    let mut command = Command::new("taskkill");
    command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_command_window(&mut command);
    let status = command.status().ok()?;
    status.success().then_some(())
}

/// A pid this process may signal at all.
///
/// `0` means "my own process group" and `-1` means "every process I am
/// allowed to signal" — both are broadcast semantics that a job's recorded
/// pid must never reach, and `1` is init. Sign-flipping a pid for a group
/// signal turns a stored `1` into exactly the `kill(-1, …)` broadcast, so
/// the floor is checked once, here.
#[cfg(unix)]
fn signalable_pid(pid: u32) -> Option<i32> {
    let pid = i32::try_from(pid).ok()?;
    (pid > 1).then_some(pid)
}

/// Signal one process, and nothing else.
#[cfg(unix)]
pub fn terminate_process(pid: u32) -> Option<()> {
    let pid = signalable_pid(pid)?;
    unix_kill(pid, libc::SIGTERM).ok()?.then_some(())
}

/// Whether the group led by `pid` is one Rebon created and may signal.
///
/// `detached` is the provenance and it comes from the job state: only a
/// process spawned through [`crate::detach_background_command`] is recorded
/// with it, and only such a process leads a group containing nothing but
/// its own children. Leading a group proves nothing on its own — a
/// terminal-launched Rebon is normally its shell job's leader, and a
/// session running in-process records *that* pid — so signalling the group
/// on leadership alone would take out the user's pipeline.
///
/// The two structural checks stay as a second gate: the process must still
/// lead its own group (a stale pid that got reused would not), and it must
/// not be the group this very process runs in.
#[cfg(unix)]
fn may_signal_process_group(pid: i32, detached: bool) -> bool {
    if !detached {
        return false;
    }
    // SAFETY: getpgid on a pid; no memory is involved. A failure returns -1,
    // which matches neither branch.
    let group = unsafe { libc::getpgid(pid) };
    let own_group = unsafe { libc::getpgid(0) };
    group == pid && group != own_group
}

/// Whether any process in the group led by `pid` is still alive.
///
/// The leader exiting does not empty its group — the tool subprocesses a
/// worker started are still in it, and they are precisely the orphans that
/// keep holding the API key and the working directory.
#[cfg(unix)]
fn process_group_has_members(pid: i32) -> bool {
    // Signal 0 performs the permission and existence checks without
    // delivering anything: success means at least one process is there.
    // SAFETY: kill with signal 0 on a group id; no memory is involved.
    unsafe { libc::kill(-pid, 0) == 0 }
}

/// Terminate a background process **and the group it leads**, escalating to
/// `SIGKILL` if the polite signal is not enough.
///
/// Workers and the supervisor are spawned into their own session
/// ([`crate::detach_background_command`]), so a worker's process-group id is
/// its own pid and that group holds the children it spawned without asking
/// for a group of their own. Signalling the group is what makes "stopping
/// the worker stops its work" true: Rust teardown never runs on a terminated
/// process, so a bare `kill <pid>` leaves that tree orphaned.
///
/// Known limit: a child that made itself a group leader (the shell and MCP
/// spawn paths both do, so they can kill their own trees) is in a different
/// group and no signal here reaches it. Those rely on their owner's
/// teardown, which a SIGKILL escalation skips by definition.
#[cfg(unix)]
fn terminate_process_group_tree(pid: u32, detached: bool, grace: Duration) -> Option<()> {
    let pid = signalable_pid(pid)?;
    let group = may_signal_process_group(pid, detached);
    let mut delivered = false;
    if group {
        // SAFETY: kill on a group id; no memory is involved.
        delivered |= unsafe { libc::kill(-pid, libc::SIGTERM) } == 0;
    }
    delivered |= unsafe { libc::kill(pid, libc::SIGTERM) } == 0;
    if !delivered && !(group && process_group_has_members(pid)) {
        return None;
    }

    // Polite first, then not. A worker that ignores SIGTERM would otherwise
    // keep the job unstoppable, and the caller's own wait would only report
    // that it never exited. The group is polled too: the leader exiting does
    // not empty it, and a surviving member is exactly the orphan this is
    // here to prevent.
    if wait_for_tree_exit(pid, group, grace) {
        return Some(());
    }
    if group {
        // SAFETY: kill on a group id; no memory is involved.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    // SIGKILL cannot be caught, so the tree goes almost at once — but not
    // instantly, and the caller checks the group afterwards. Waiting a beat
    // here is what makes that check mean "still running" rather than "not
    // finished dying yet".
    wait_for_tree_exit(pid, group, KILL_DRAIN_BUDGET);
    Some(())
}

/// How long to wait for a `SIGKILL`ed tree to actually go.
#[cfg(unix)]
const KILL_DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// Wait until the leader is gone and, when it owns one, its group is empty.
/// `true` if the tree drained within `budget`.
#[cfg(unix)]
fn wait_for_tree_exit(pid: i32, group: bool, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        // Zombie-aware where the platform can tell: a leader whose parent
        // has not reaped it yet has exited, and waiting for its entry to
        // disappear would spend the whole budget for nothing.
        let leader_gone = process_is_running(pid as u32) == Some(false);
        if leader_gone && !(group && process_group_has_members(pid)) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(not(any(unix, windows)))]
pub fn terminate_process(_pid: u32) -> Option<()> {
    None
}

/// Whether a Linux pid belongs to a process that is still *running*.
///
/// `/proc/<pid>` existing is not that question: an exited child whose
/// parent has not reaped it stays there as a zombie. Reading it as alive is
/// how a dead supervisor keeps a fresh roster entry — the gate never
/// respawns, and every new job sits in `Queued` forever. The state field in
/// `stat` is the one that answers.
#[cfg(target_os = "linux")]
pub fn process_is_running(pid: u32) -> Option<bool> {
    let path = std::path::PathBuf::from(format!("/proc/{pid}/stat"));
    match std::fs::read_to_string(&path) {
        Ok(stat) => parse_linux_proc_stat(&stat)
            .ok()
            .map(|(state, _process_group)| state != 'Z'),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        // Preserve the baseline's honest existence answer when procfs denies
        // the stat read, while malformed readable data still fails closed.
        Err(_) => Some(path.parent().is_some_and(std::path::Path::exists)),
    }
}

/// `stat` state field: `pid (comm) S …`. `comm` is parenthesised and may
/// itself contain spaces and parentheses, so the state is parsed after the
/// LAST `)`.
#[cfg(target_os = "linux")]
fn linux_stat_is_zombie(stat: &str) -> bool {
    parse_linux_proc_stat(stat).is_ok_and(|(state, _)| state == 'Z')
}

#[cfg(all(unix, not(target_os = "linux")))]
pub fn process_is_running(pid: u32) -> Option<bool> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    match unix_kill(pid, 0) {
        Ok(exists) => Some(exists),
        // EPERM means the target exists but is unsignalable. unix_kill keeps
        // that distinct from ESRCH; liveness must report it as present.
        Err(error) if error.to_string().contains("exists but cannot be signalled") => Some(true),
        Err(_) => None,
    }
}

/// Asked of the kernel, not of `tasklist`: this used to spawn
/// `tasklist /FI "PID eq N"` — a process start of 300 ms or more, and it
/// sits on the path of every job listing, every stale-pid reconcile and
/// the probe a terminal makes while waiting to mirror its worker, on the
/// UI thread. `OpenProcess` answers in microseconds, and a process that
/// exists but is not ours to open (another user's) says so with
/// `ERROR_ACCESS_DENIED`, which is still "running".
#[cfg(windows)]
pub fn process_is_running(pid: u32) -> Option<bool> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    /// What `GetExitCodeProcess` reports for a process that has not exited.
    const STILL_ACTIVE: u32 = 259;

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle == 0 {
        return match unsafe { GetLastError() } {
            ERROR_INVALID_PARAMETER => Some(false),
            ERROR_ACCESS_DENIED => Some(true),
            _ => None,
        };
    }
    let mut exit_code = 0u32;
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe {
        CloseHandle(handle);
    }
    // A process that exited but whose pid is still held open by someone
    // answers with its exit code: gone, whatever the pid table says.
    queried.then_some(exit_code == STILL_ACTIVE)
}

#[cfg(not(any(unix, windows)))]
pub fn process_is_running(_pid: u32) -> Option<bool> {
    None
}

#[cfg(target_os = "linux")]
pub fn process_identity(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let command_end = stat.rfind(')')?;
    let started_at = stat[command_end + 1..].split_whitespace().nth(19)?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("linux:{}:{started_at}", boot_id.trim()))
}

#[cfg(target_os = "macos")]
// Matches `struct proc_bsdinfo` from the macOS libproc headers.
#[allow(dead_code)]
#[repr(C)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [std::ffi::c_char; 16],
    pbi_name: [std::ffi::c_char; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
extern "C" {
    fn proc_pidinfo(
        pid: std::ffi::c_int,
        flavor: std::ffi::c_int,
        arg: u64,
        buffer: *mut std::ffi::c_void,
        buffersize: std::ffi::c_int,
    ) -> std::ffi::c_int;

    fn proc_pidpath(
        pid: std::ffi::c_int,
        buffer: *mut std::ffi::c_void,
        buffersize: u32,
    ) -> std::ffi::c_int;
}

#[cfg(target_os = "macos")]
pub fn process_identity(pid: u32) -> Option<String> {
    const PROC_PIDTBSDINFO: std::ffi::c_int = 3;

    let pid = std::ffi::c_int::try_from(pid).ok()?;
    let buffer_size = std::mem::size_of::<ProcBsdInfo>();
    let buffer_size = std::ffi::c_int::try_from(buffer_size).ok()?;
    let mut info = std::mem::MaybeUninit::<ProcBsdInfo>::uninit();
    // SAFETY: `info` points to a buffer of the exact size passed to libproc. We only
    // initialize it after libproc reports that it filled the complete structure.
    let bytes_written = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if bytes_written != buffer_size {
        return None;
    }
    // SAFETY: A full-size successful result initialized every byte in `info`.
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid as u32 || info.pbi_start_tvsec == 0 || info.pbi_start_tvusec >= 1_000_000
    {
        return None;
    }
    Some(format!(
        "macos:{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn process_identity(_pid: u32) -> Option<String> {
    None
}

#[cfg(windows)]
pub fn process_identity(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle == 0 {
        return None;
    }
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    let succeeded =
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0 };
    unsafe {
        CloseHandle(handle);
    }
    if !succeeded {
        return None;
    }
    let started_at = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
    Some(format!("windows:{started_at}"))
}

#[cfg(not(any(unix, windows)))]
pub fn process_identity(_pid: u32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
pub fn process_executable_path(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
pub fn process_executable_path(pid: u32) -> Option<std::path::PathBuf> {
    // Matches PROC_PIDPATHINFO_MAXSIZE (4 * MAXPATHLEN) from the libproc headers;
    // proc_pidpath fails outright with a smaller buffer.
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

    let pid = std::ffi::c_int::try_from(pid).ok()?;
    let mut buffer = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: `buffer` outlives the call and its exact length is passed alongside.
    let len = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
    let path = std::str::from_utf8(&buffer[..len]).ok()?;
    Some(std::path::PathBuf::from(path))
}

#[cfg(windows)]
pub fn process_executable_path(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle == 0 {
        return None;
    }
    // Extended-length paths cap at 32767 UTF-16 units.
    let mut buffer = vec![0u16; 32768];
    let mut len = buffer.len() as u32;
    let succeeded = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len) != 0
    };
    unsafe {
        CloseHandle(handle);
    }
    if !succeeded {
        return None;
    }
    let path = std::ffi::OsString::from_wide(&buffer[..len as usize]);
    Some(std::path::PathBuf::from(path))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn process_executable_path(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_liveness_reports_current_process_alive() {
        assert_eq!(process_is_running(std::process::id()), Some(true));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn process_identity_distinguishes_pid_reuse() {
        let pid = std::process::id();
        let identity = process_identity(pid).expect("current process identity");
        assert!(recorded_process_is_running(pid, Some(&identity)).unwrap());
        assert!(!recorded_process_is_running(pid, Some("different-process-instance")).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_process_identity_uses_kernel_start_time_with_microseconds() {
        assert_eq!(std::mem::size_of::<ProcBsdInfo>(), 136);

        let identity = process_identity(std::process::id()).expect("current process identity");
        let start = identity
            .strip_prefix("macos:")
            .expect("macOS identity prefix");
        let (seconds, microseconds) = start.split_once(':').expect("start time fields");
        assert!(seconds.parse::<u64>().expect("start seconds") > 0);
        assert!(microseconds.parse::<u64>().expect("start microseconds") < 1_000_000);
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    #[test]
    fn unsupported_unix_process_identity_fails_closed() {
        let pid = std::process::id();
        assert_eq!(process_identity(pid), None);
        assert!(recorded_process_is_running(pid, Some("legacy-unix-identity")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_stat_parser_handles_spaces_parentheses_and_zombies() {
        let (state, group) =
            parse_linux_proc_stat("42 (worker name (nested)) Z 1 42 42 0").unwrap();
        assert_eq!(state, 'Z');
        assert_eq!(group, 42);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_zombie_is_not_reported_as_a_running_process() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.05"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let identity = process_identity(pid).expect("child process identity");
        let became_zombie = (0..100).any(|_| {
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| parse_linux_proc_stat(&stat).ok())
                .is_some_and(|(state, _)| state == 'Z');
            if !zombie {
                std::thread::sleep(Duration::from_millis(10));
            }
            zombie
        });

        assert!(became_zombie, "child never reached zombie state");
        assert_eq!(process_is_running(pid), Some(false));
        assert!(!recorded_process_is_running(pid, Some(&identity)).unwrap());
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn current_process_group_probe_matches_kernel_group_id() {
        let pid = std::process::id();
        let expected = unsafe { libc::getpgid(libc::pid_t::try_from(pid).unwrap()) }
            == libc::pid_t::try_from(pid).unwrap();
        assert_eq!(process_owns_detached_group(pid), expected);
    }

    #[cfg(unix)]
    #[test]
    fn unix_kill_error_classification_distinguishes_absent_and_unsignalable() {
        assert!(
            !classify_unix_kill_error(42, std::io::Error::from_raw_os_error(libc::ESRCH),).unwrap()
        );
        let denied = classify_unix_kill_error(42, std::io::Error::from_raw_os_error(libc::EPERM))
            .unwrap_err();
        assert!(denied
            .to_string()
            .contains("exists but cannot be signalled"));
    }

    #[cfg(unix)]
    #[test]
    fn group_drain_ignores_zombies_but_keeps_other_states() {
        assert_eq!(
            active_members(vec![
                UnixGroupMember { pid: 1, state: 'Z' },
                UnixGroupMember { pid: 2, state: 'S' },
                UnixGroupMember { pid: 3, state: 'R' },
            ]),
            vec![2, 3]
        );
    }

    #[cfg(unix)]
    fn detached_command(script: &str) -> std::process::Command {
        use std::os::unix::process::CommandExt;

        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", script])
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mismatched_zombie_leader_identity_is_rejected_before_group_signalling() {
        let mut child = detached_command("sleep 0.05").spawn().unwrap();
        let pid = child.id();
        let became_zombie = (0..100).any(|_| {
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| parse_linux_proc_stat(&stat).ok())
                .is_some_and(|(state, _)| state == 'Z');
            if !zombie {
                std::thread::sleep(Duration::from_millis(10));
            }
            zombie
        });

        assert!(became_zombie, "group leader never reached zombie state");
        let error = terminate_recorded_process_tree(
            pid,
            Some("different-process-instance"),
            true,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity does not match"));
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detached_group_termination_drains_leader_and_children() {
        let mut child = detached_command("sleep 30 & wait").spawn().unwrap();
        let pid = child.id();
        let identity = process_identity(pid).expect("group leader identity");

        let result =
            terminate_recorded_process_tree(pid, Some(&identity), true, Duration::from_secs(5));
        if let Err(error) = result {
            let _ = unix_kill(-(pid as libc::pid_t), libc::SIGKILL);
            let _ = child.wait();
            panic!("failed to terminate detached process group: {error}");
        }
        let _ = child.wait();
        assert!(active_unix_group_members(pid).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn mismatched_live_leader_identity_is_never_group_signalled() {
        let mut child = detached_command("sleep 30 & wait").spawn().unwrap();
        let pid = child.id();
        let identity = process_identity(pid).expect("group leader identity");

        let error = terminate_recorded_process_tree(
            pid,
            Some("different-process-instance"),
            true,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity does not match"));
        assert!(child.try_wait().unwrap().is_none());

        terminate_recorded_process_tree(pid, Some(&identity), true, Duration::from_secs(5))
            .unwrap();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn detached_group_termination_handles_dead_leader_with_surviving_member() {
        let dir = tempfile::tempdir().unwrap();
        let child_pid_path = dir.path().join("child-pid");
        let script = format!(
            "sleep 30 & echo $! > '{}'; exit 0",
            child_pid_path.display()
        );
        let mut leader = detached_command(&script).spawn().unwrap();
        let leader_pid = leader.id();
        let identity = process_identity(leader_pid).expect("group leader identity");
        leader.wait().unwrap();
        let member_pid = (0..100)
            .find_map(|_| {
                std::fs::read_to_string(&child_pid_path)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    .or_else(|| {
                        std::thread::sleep(Duration::from_millis(10));
                        None
                    })
            })
            .expect("surviving group member pid");
        assert!(active_unix_group_members(leader_pid)
            .unwrap()
            .contains(&member_pid));

        if let Err(error) = terminate_recorded_process_tree(
            leader_pid,
            Some(&identity),
            true,
            Duration::from_secs(5),
        ) {
            let _ = unix_kill(-(leader_pid as libc::pid_t), libc::SIGKILL);
            panic!("failed to terminate group after leader exit: {error}");
        }
        assert!(active_unix_group_members(leader_pid).unwrap().is_empty());
    }

    #[test]
    fn wait_for_pid_exit_fails_when_process_remains_alive() {
        let error = wait_for_pid_exit(std::process::id(), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("did not exit"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn process_executable_path_reports_current_executable() {
        let reported =
            process_executable_path(std::process::id()).expect("current process executable path");
        let current = std::env::current_exe().expect("current_exe");
        assert_eq!(
            reported.canonicalize().expect("canonicalize reported path"),
            current.canonicalize().expect("canonicalize current_exe"),
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn process_executable_path_fails_for_dead_pid() {
        // Probe downward from just below i32::MAX: far above any real pid on
        // every platform, yet inside the range Windows tasklist (which backs
        // process_is_running) accepts — it rejects pid filters above i32::MAX
        // as invalid queries, answering None forever instead of Some(false).
        let dead_pid = (0..64)
            .map(|offset| i32::MAX as u32 - 1 - offset)
            .find(|&pid| process_is_running(pid) == Some(false))
            .expect("no dead pid found just below i32::MAX");
        assert_eq!(process_executable_path(dead_pid), None);
    }

    /// The provenance rule, in one place: a recorded identity is not proof
    /// that a pid leads a group Rebon made. An in-process session records
    /// the *interactive* pid, and a terminal-launched Rebon is normally its
    /// shell job's group leader — signalling that group would take out the
    /// user's pipeline.
    #[cfg(unix)]
    #[test]
    fn a_group_is_only_signalled_with_recorded_provenance() {
        let own = std::process::id() as i32;
        assert!(
            !may_signal_process_group(own, false),
            "without the detached-spawn flag no group may be signalled, however the pid looks"
        );
        // And even with it, never the group this process is running in.
        assert!(
            !may_signal_process_group(own, true),
            "our own process group is never a target"
        );
    }

    /// Broadcast pids are not job owners. Sign-flipping for a group signal
    /// turns a stored `1` into `kill(-1, …)` — every process we may signal.
    #[cfg(unix)]
    #[test]
    fn broadcast_pids_are_refused() {
        assert_eq!(signalable_pid(0), None);
        assert_eq!(signalable_pid(1), None);
        assert_eq!(signalable_pid(2), Some(2));
        assert_eq!(terminate_process(0), None);
        assert_eq!(terminate_process(1), None);
    }

    /// A zombie is an exited process, and reading it as alive is what kept
    /// a dead supervisor's roster entry looking fresh.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_zombie_state_is_not_running() {
        // `comm` is parenthesised and may contain spaces and parentheses,
        // so the state has to be read after the LAST ')'.
        assert!(linux_stat_is_zombie("42 (rebon) Z 1 42 42 0 -1 4194560 0"));
        assert!(linux_stat_is_zombie("42 (we (ir) d name) Z 1 42 42"));
        assert!(!linux_stat_is_zombie("42 (rebon) S 1 42 42 0 -1 4194560 0"));
        assert!(!linux_stat_is_zombie("42 (Z) R 1 42 42"));
        assert!(!linux_stat_is_zombie("malformed"));
    }
}
