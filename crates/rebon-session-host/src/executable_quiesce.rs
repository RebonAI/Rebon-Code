//! Best-effort termination of roster-owned processes running a given executable.
//!
//! Used by the desktop app right before a self-update install. The bundled
//! `rebon` sidecar hosts both detached background processes and interactive
//! TUI sessions. Only supervisor/worker pids recorded in the background roster
//! may be terminated; a separate read-only process-table scan lets the updater
//! detect any remaining interactive process and defer installation instead of
//! closing it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process_liveness::{
    process_executable_path, process_is_running, terminate_process_best_effort,
    terminate_recorded_process_tree,
};
use crate::BackgroundRoster;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecutableQuiesceReport {
    pub terminated: Vec<u32>,
    pub failed: Vec<u32>,
}

impl ExecutableQuiesceReport {
    pub fn is_empty(&self) -> bool {
        self.terminated.is_empty() && self.failed.is_empty()
    }
}

/// Terminate roster-owned processes whose executable image is `executable`.
///
/// The supervisor is tried before its workers so it cannot respawn a worker
/// mid-quiesce. Unregistered processes are deliberately excluded: the bundled
/// executable can also host an interactive TUI that must survive an app update.
pub fn quiesce_roster_processes_running_executable(
    roster: Option<&BackgroundRoster>,
    executable: &Path,
    per_process_timeout: Duration,
) -> ExecutableQuiesceReport {
    let candidates = roster_candidate_pids(roster);
    let (targets, inspection_failures) = inspect_roster_candidates_running_executable(
        candidates,
        executable,
        std::process::id(),
        process_executable_path,
        process_is_running,
    );

    let mut report = ExecutableQuiesceReport {
        terminated: Vec::new(),
        failed: inspection_failures,
    };
    for pid in targets {
        let Some((identity, owner_detached_group)) = roster_candidate_owner(roster, pid) else {
            report.failed.push(pid);
            continue;
        };
        let Some(identity) = identity else {
            tracing::warn!(
                pid,
                "refusing to quiesce roster process without a recorded identity"
            );
            report.failed.push(pid);
            continue;
        };
        match terminate_recorded_process_tree(
            pid,
            Some(identity),
            owner_detached_group,
            per_process_timeout,
        ) {
            Ok(()) => report.terminated.push(pid),
            Err(_) if !owner_detached_group && process_is_running(pid) == Some(false) => {
                report.terminated.push(pid)
            }
            Err(error) => {
                tracing::warn!(pid, %error, "failed to quiesce process during update");
                report.failed.push(pid);
            }
        }
    }
    report
}

/// Terminate every process in `candidates` that still runs `executable`, after
/// re-verifying each candidate's executable path at terminate time.
///
/// Unlike [`quiesce_roster_processes_running_executable`] this is not limited
/// to roster-owned pids: the desktop updater calls it with the pids of the
/// remaining interactive CLI/TUI sessions once the user has approved closing
/// them. The per-candidate path probe is what makes the approval safe — a pid
/// that has been recycled into an unrelated process is filtered out, never
/// terminated. Callers should re-run [`pids_running_executable`] afterwards and
/// treat anything still running as unapproved.
pub fn terminate_processes_running_executable(
    candidates: Vec<u32>,
    executable: &Path,
    per_process_timeout: Duration,
) -> ExecutableQuiesceReport {
    terminate_selected_pids(
        select_pids_running_executable(
            candidates,
            executable,
            std::process::id(),
            process_executable_path,
        ),
        per_process_timeout,
    )
}

fn terminate_selected_pids(
    targets: Vec<u32>,
    per_process_timeout: Duration,
) -> ExecutableQuiesceReport {
    let mut report = ExecutableQuiesceReport::default();
    for pid in targets {
        match terminate_process_best_effort(pid, per_process_timeout) {
            Ok(()) => report.terminated.push(pid),
            Err(_) if process_is_running(pid) == Some(false) => report.terminated.push(pid),
            Err(error) => {
                tracing::warn!(pid, %error, "failed to quiesce process during update");
                report.failed.push(pid);
            }
        }
    }
    report
}

/// Return every process currently running `executable` without terminating it.
///
/// On Windows the process table is swept by image name and every candidate's
/// full path is verified. If a same-name live process cannot be inspected, the
/// scan fails so callers can defer an update instead of risking a false negative.
pub fn pids_running_executable(executable: &Path) -> anyhow::Result<Vec<u32>> {
    inspect_pids_running_executable(
        pids_with_image_name(executable.file_name().unwrap_or_default())?,
        executable,
        process_executable_path,
        process_is_running,
    )
}

fn roster_candidate_pids(roster: Option<&BackgroundRoster>) -> Vec<u32> {
    let mut candidates = Vec::new();
    if let Some(roster) = roster {
        candidates.push(roster.supervisor_pid);
        candidates.extend(roster.jobs.iter().filter_map(|job| job.pid));
    }
    candidates
}

fn roster_candidate_owner(
    roster: Option<&BackgroundRoster>,
    pid: u32,
) -> Option<(Option<&str>, bool)> {
    let roster = roster?;
    if roster.supervisor_pid == pid {
        return Some((roster.supervisor_pid_identity.as_deref(), false));
    }
    roster.jobs.iter().find_map(|job| {
        (job.pid == Some(pid)).then_some((job.pid_identity.as_deref(), job.owner_detached_group))
    })
}

/// Pure candidate filter: keep pids (input order, deduplicated) whose probed
/// executable path is `executable`, skipping the calling process and pid 0.
///
/// The roster path classifies instead (an unreadable live candidate is a
/// failure there, not an absence). This plain filter is what the approved
/// desktop-updater path wants: those pids were named by the user, carry no
/// roster identity, and a pid that no longer runs the executable is simply
/// not one of them.
fn select_pids_running_executable(
    candidates: Vec<u32>,
    executable: &Path,
    current_pid: u32,
    probe: impl Fn(u32) -> Option<PathBuf>,
) -> Vec<u32> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|&pid| pid != 0 && pid != current_pid && seen.insert(pid))
        .filter(|&pid| probe(pid).is_some_and(|running| same_executable_path(&running, executable)))
        .collect()
}

/// Classify every roster candidate so an unreadable live process is carried to
/// the caller as a quiesce failure rather than disappearing from the stop tree.
fn inspect_roster_candidates_running_executable(
    candidates: Vec<u32>,
    executable: &Path,
    current_pid: u32,
    probe: impl Fn(u32) -> Option<PathBuf>,
    is_running: impl Fn(u32) -> Option<bool>,
) -> (Vec<u32>, Vec<u32>) {
    let mut seen = HashSet::new();
    let mut matching = Vec::new();
    let mut failed = Vec::new();
    for pid in candidates
        .into_iter()
        .filter(|&pid| pid != 0 && pid != current_pid && seen.insert(pid))
    {
        match probe(pid) {
            Some(running) if same_executable_path(&running, executable) => matching.push(pid),
            Some(_) => {}
            None if is_running(pid) == Some(false) => {}
            None => failed.push(pid),
        }
    }
    (matching, failed)
}

fn inspect_pids_running_executable(
    candidates: Vec<u32>,
    executable: &Path,
    probe: impl Fn(u32) -> Option<PathBuf>,
    is_running: impl Fn(u32) -> Option<bool>,
) -> anyhow::Result<Vec<u32>> {
    let mut seen = HashSet::new();
    let mut matching = Vec::new();
    for pid in candidates
        .into_iter()
        .filter(|&pid| pid != 0 && seen.insert(pid))
    {
        match probe(pid) {
            Some(running) if same_executable_path(&running, executable) => matching.push(pid),
            Some(_) => {}
            None if is_running(pid) == Some(false) => {}
            None => anyhow::bail!("could not verify executable path for live process {pid}"),
        }
    }
    Ok(matching)
}

/// Path equality that survives the probe and the caller resolving the same
/// image differently (`\\?\`-prefixed vs plain, case differences on Windows).
fn same_executable_path(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    normalized_path_string(a) == normalized_path_string(b)
}

fn normalized_path_string(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = text.strip_prefix("\\\\?\\").unwrap_or(&text);
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_string()
    }
}

#[cfg(windows)]
fn pids_with_image_name(name: &std::ffi::OsStr) -> anyhow::Result<Vec<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let wanted = name.to_string_lossy().to_lowercase();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: PROCESSENTRY32W is plain data; dwSize tells the API the exact
    // struct size it may write, and the snapshot handle stays open for the walk.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut pids = Vec::new();
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let name_len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]).to_lowercase();
        if name == wanted {
            pids.push(entry.th32ProcessID);
        }
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }
    Ok(pids)
}

#[cfg(not(windows))]
fn pids_with_image_name(_name: &std::ffi::OsStr) -> anyhow::Result<Vec<u32>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{BackgroundJobStatus, BackgroundRosterJob};

    fn probe_from<'a>(map: &'a HashMap<u32, &'static str>) -> impl Fn(u32) -> Option<PathBuf> + 'a {
        move |pid| map.get(&pid).map(PathBuf::from)
    }

    #[test]
    fn select_keeps_only_pids_running_the_executable() {
        let exe = Path::new("/opt/rebon/rebon-cli");
        let probe = HashMap::from([
            (10, "/opt/rebon/rebon-cli"),
            (11, "/usr/bin/other"),
            (12, "/opt/rebon/rebon-cli"),
        ]);

        let selected =
            select_pids_running_executable(vec![10, 11, 12, 13], exe, 999, probe_from(&probe));

        assert_eq!(selected, vec![10, 12]);
    }

    #[test]
    fn select_skips_current_process_pid_zero_and_duplicates() {
        let exe = Path::new("/opt/rebon/rebon-cli");
        let probe = HashMap::from([(10, "/opt/rebon/rebon-cli"), (20, "/opt/rebon/rebon-cli")]);

        let selected =
            select_pids_running_executable(vec![0, 20, 10, 10, 20], exe, 20, probe_from(&probe));

        assert_eq!(selected, vec![10]);
    }

    #[test]
    fn select_preserves_candidate_order_supervisor_first() {
        let exe = Path::new("/opt/rebon/rebon-cli");
        let probe = HashMap::from([
            (7, "/opt/rebon/rebon-cli"),
            (3, "/opt/rebon/rebon-cli"),
            (5, "/opt/rebon/rebon-cli"),
        ]);

        // Roster ordering: supervisor pid leads, worker pids follow.
        let selected = select_pids_running_executable(vec![7, 3, 5], exe, 999, probe_from(&probe));

        assert_eq!(selected, vec![7, 3, 5]);
    }

    #[test]
    fn roster_candidates_exclude_unregistered_same_executable_pid() {
        let roster = BackgroundRoster {
            supervisor_pid: 7,
            supervisor_pid_identity: Some("supervisor-identity".into()),
            updated_at_ms: 0,
            jobs: vec![BackgroundRosterJob {
                job_id: "job".into(),
                session_id: None,
                cwd: ".".into(),
                status: BackgroundJobStatus::Running,
                pid: Some(3),
                pid_identity: Some("worker-identity".into()),
                owner_detached_group: false,
                updated_at_ms: 0,
            }],
        };
        let exe = Path::new("/opt/rebon/rebon-cli");
        let probe = HashMap::from([
            (7, "/opt/rebon/rebon-cli"),
            (3, "/opt/rebon/rebon-cli"),
            (99, "/opt/rebon/rebon-cli"),
        ]);

        let selected = select_pids_running_executable(
            roster_candidate_pids(Some(&roster)),
            exe,
            999,
            probe_from(&probe),
        );

        assert_eq!(selected, vec![7, 3]);
    }

    #[test]
    fn inspect_fails_closed_when_live_candidate_path_cannot_be_read() {
        let result = inspect_pids_running_executable(
            vec![10],
            Path::new("/opt/rebon/rebon-cli"),
            |_| None,
            |_| Some(true),
        );

        assert!(result.is_err());
    }

    #[test]
    fn inspect_ignores_candidate_that_exited_during_scan() {
        let result = inspect_pids_running_executable(
            vec![10],
            Path::new("/opt/rebon/rebon-cli"),
            |_| None,
            |_| Some(false),
        )
        .unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn roster_inspection_reports_unknown_live_candidates_as_failures() {
        let executable = Path::new("/opt/rebon/rebon-cli");
        let probe = HashMap::from([(10, "/opt/rebon/rebon-cli"), (13, "/usr/bin/other")]);

        let (matching, failed) = inspect_roster_candidates_running_executable(
            vec![0, 10, 11, 12, 13, 10, 99],
            executable,
            99,
            probe_from(&probe),
            |pid| match pid {
                11 => Some(true),
                12 => Some(false),
                _ => Some(true),
            },
        );

        assert_eq!(matching, vec![10]);
        assert_eq!(failed, vec![11]);
    }

    #[cfg(windows)]
    #[test]
    fn same_executable_path_is_case_insensitive_on_windows() {
        assert!(same_executable_path(
            Path::new(r"C:\Apps\Rebon\rebon-cli.exe"),
            Path::new(r"c:\apps\rebon\REBON-CLI.EXE"),
        ));
        assert!(same_executable_path(
            Path::new(r"\\?\C:\Apps\Rebon\rebon-cli.exe"),
            Path::new(r"C:\Apps\Rebon\rebon-cli.exe"),
        ));
    }

    #[test]
    fn same_executable_path_rejects_different_images() {
        assert!(!same_executable_path(
            Path::new("/opt/rebon/rebon-cli"),
            Path::new("/opt/rebon/other"),
        ));
    }

    #[test]
    fn same_executable_path_matches_canonicalized_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rebon-cli");
        std::fs::write(&exe, b"").unwrap();
        let indirect = dir.path().join(".").join("rebon-cli");

        assert!(same_executable_path(&indirect, &exe));
    }

    #[test]
    fn quiesce_with_no_matching_processes_reports_empty() {
        let roster = BackgroundRoster {
            supervisor_pid: std::process::id(),
            supervisor_pid_identity: crate::process_identity(std::process::id()),
            updated_at_ms: 0,
            jobs: vec![BackgroundRosterJob {
                job_id: "job".into(),
                session_id: None,
                cwd: ".".into(),
                status: BackgroundJobStatus::Running,
                pid: Some(std::process::id()),
                pid_identity: crate::process_identity(std::process::id()),
                owner_detached_group: false,
                updated_at_ms: 0,
            }],
        };

        // Every candidate is the current process, which is always skipped, so
        // nothing can be terminated even though the image path would match.
        let report = quiesce_roster_processes_running_executable(
            Some(&roster),
            &std::env::current_exe().unwrap(),
            Duration::from_millis(100),
        );

        assert!(report.is_empty());
    }

    #[test]
    fn roster_owner_uses_supervisor_identity_before_duplicate_worker_pid() {
        let roster = BackgroundRoster {
            supervisor_pid: 7,
            supervisor_pid_identity: Some("supervisor".into()),
            updated_at_ms: 0,
            jobs: vec![BackgroundRosterJob {
                job_id: "job".into(),
                session_id: None,
                cwd: ".".into(),
                status: BackgroundJobStatus::Running,
                pid: Some(7),
                pid_identity: Some("worker".into()),
                owner_detached_group: true,
                updated_at_ms: 0,
            }],
        };

        assert_eq!(
            roster_candidate_owner(Some(&roster), 7),
            Some((Some("supervisor"), false))
        );
    }

    #[test]
    fn legacy_roster_owner_has_no_identity_and_cannot_be_safely_signalled() {
        let json = r#"{"supervisorPid":7,"updatedAtMs":0,"jobs":[]}"#;
        let roster: BackgroundRoster = serde_json::from_str(json).unwrap();

        assert_eq!(
            roster_candidate_owner(Some(&roster), 7),
            Some((None, false))
        );
    }

    #[test]
    fn approved_termination_skips_pids_that_do_not_run_the_executable() {
        // A recycled pid must never be terminated: candidates whose probed path
        // does not match the executable are filtered before any termination.
        let report = terminate_processes_running_executable(
            vec![0, 1, 2],
            std::path::Path::new("/opt/rebon/rebon-cli"),
            Duration::from_millis(10),
        );

        assert!(report.is_empty());
    }

    #[test]
    fn approved_termination_skips_the_calling_process() {
        let current = std::env::current_exe().unwrap();
        let report = terminate_processes_running_executable(
            vec![std::process::id()],
            &current,
            Duration::from_millis(10),
        );

        assert!(report.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_image_name_scan_is_case_insensitive() {
        let current = std::env::current_exe().unwrap();
        let upper = current
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_uppercase();
        let pids = pids_with_image_name(std::ffi::OsStr::new(&upper)).unwrap();
        assert!(pids.contains(&std::process::id()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_executable_scan_finds_current_process() {
        let current = std::env::current_exe().unwrap();
        let pids = pids_running_executable(&current).unwrap();
        assert!(pids.contains(&std::process::id()));
    }
}
