#![cfg(feature = "adversarial-fixtures")]

use rebon_boa_runner::{IsolatedJsRunner, Limits, RunnerError, StatusScriptRequest};
use serde_json::json;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const WATCHDOG_WALL_TIME: Duration = Duration::from_millis(400);
const WATCHDOG_SETTLE_MARGIN: Duration = Duration::from_millis(200);
const FINAL_MARKER_MARGIN: Duration = Duration::from_millis(750);
const SUPERVISOR_GATE_TIMEOUT: Duration = Duration::from_secs(4);
const GATED_MARKER_READY_PREFIX: &str = "gated-marker-ready:";

struct StallRelease {
    path: PathBuf,
    released: bool,
}

impl StallRelease {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            released: false,
        }
    }

    fn release(&mut self) {
        write_marker(&self.path, b"release");
        self.released = true;
    }
}

impl Drop for StallRelease {
    fn drop(&mut self) {
        if !self.released {
            let _ = std::fs::write(&self.path, b"release");
        }
    }
}

fn production() -> (IsolatedJsRunner, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let runner = IsolatedJsRunner::new(
        PathBuf::from(env!("CARGO_BIN_EXE_rebon-boa-test-helper")),
        directory.path().canonicalize().unwrap(),
    )
    .unwrap();
    (runner, directory)
}

fn fixture(
    mode: &str,
    extra: &[OsString],
    active_process_limit: u32,
) -> (IsolatedJsRunner, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let mut args = vec![OsString::from(mode)];
    args.extend_from_slice(extra);
    let runner = IsolatedJsRunner::new_test_fixture(
        PathBuf::from(env!("CARGO_BIN_EXE_rebon-boa-adversarial-fixture")),
        directory.path().canonicalize().unwrap(),
        args,
        active_process_limit,
    )
    .unwrap();
    (runner, directory)
}

fn limits(milliseconds: u64) -> Limits {
    Limits {
        wall_time: Duration::from_millis(milliseconds),
        ..Limits::default()
    }
}

fn timeout_pid(error: RunnerError) -> u32 {
    match error {
        RunnerError::Timeout { child_pid } => child_pid,
        other => panic!("expected wall-clock timeout, got {other:?}"),
    }
}

fn request(code: &str) -> StatusScriptRequest {
    StatusScriptRequest {
        code: code.into(),
        payload: json!({"padding": "x".repeat(1024)}),
    }
}

fn write_marker(path: &Path, contents: &[u8]) {
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(contents).unwrap();
    file.flush().unwrap();
}

fn wait_for_marker(path: &Path, expected: &[u8], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut observed = None;
    loop {
        match std::fs::read(path) {
            Ok(contents) if contents == expected => return,
            Ok(contents) => observed = Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("failed to read marker {}: {error}", path.display()),
        }
        assert!(
            Instant::now() < deadline,
            "marker {} did not contain {expected:?} before timeout; observed {observed:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_gated_fixture_readiness(path: &Path, timeout: Duration) -> (u32, Instant) {
    let deadline = Instant::now() + timeout;
    let mut observed = None;
    loop {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                observed = Some(contents.clone());
                if let Some(pid) = contents
                    .strip_prefix(GATED_MARKER_READY_PREFIX)
                    .and_then(|value| value.parse().ok())
                {
                    return (pid, Instant::now());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("failed to read readiness {}: {error}", path.display()),
        }
        assert!(
            Instant::now() < deadline,
            "readiness {} did not publish a fixture PID before timeout; observed {observed:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_until_after(started: Instant, duration: Duration) {
    if let Some(remaining) = duration.checked_sub(started.elapsed()) {
        std::thread::sleep(remaining);
    }
}

fn read_optional_marker(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("failed to read marker {}: {error}", path.display()),
    }
}

#[test]
fn timeout_and_complete_response_edge_has_no_partial_success() {
    let (runner, _dir) = production();
    for _ in 0..100 {
        match runner.run(
            request("return 1;"),
            Limits {
                wall_time: Duration::from_millis(20),
                ..Limits::default()
            },
        ) {
            Ok(output) => assert_eq!(output.value, json!(1)),
            Err(RunnerError::Timeout { .. }) => {}
            Err(other) => panic!("unexpected deadline-edge outcome: {other:?}"),
        }
    }
}

#[test]
fn cancellation_during_blocking_execution_is_bounded() {
    let (runner, _dir) = fixture("blocking-native", &[], 1);
    for _ in 0..10 {
        let cancelled = AtomicBool::new(false);
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                cancelled.store(true, Ordering::Release);
            });
            runner.run_with_cancel(
                request("return 1;"),
                Limits {
                    wall_time: Duration::from_secs(1),
                    ..Limits::default()
                },
                &cancelled,
            )
        })
        .unwrap_err();
        let pid = match error {
            RunnerError::Cancelled { child_pid } => child_pid,
            other => panic!("expected cancellation, got {other:?}"),
        };
        assert_process_gone(pid);
    }
}

#[test]
fn hard_termination_covers_boa_jobs_and_blocking_execution() {
    for mode in ["boa-microtask-self-renew", "blocking-native"] {
        let (runner, _dir) = fixture(mode, &[], 1);
        for _ in 0..3 {
            let pid = timeout_pid(runner.run(request("return 1;"), limits(100)).unwrap_err());
            assert_process_gone(pid);
        }
    }
    let (runner, _dir) = production();
    assert_eq!(
        runner
            .run(request("return 7;"), Limits::default())
            .unwrap()
            .value,
        json!(7)
    );
}

#[test]
fn cpu_backstop_does_not_preempt_wall_clock_timeout() {
    let (runner, _dir) = fixture("cpu-spin", &[], 1);
    let started = Instant::now();
    let pid = timeout_pid(runner.run(request("return 1;"), limits(1_250)).unwrap_err());
    assert!(started.elapsed() >= Duration::from_millis(1_200));
    assert_process_gone(pid);
}

#[test]
fn fixed_handoff_corruption_never_becomes_success() {
    for mode in [
        "exit-no-response",
        "missing-completion",
        "bad-checksum",
        "oversized-length",
        "invalid-layout",
        "grown-file",
        "malformed-json",
        "wrong-version",
    ] {
        let (runner, _dir) = fixture(mode, &[], 1);
        assert!(
            matches!(
                runner.run(request("return 1;"), limits(500)),
                Err(RunnerError::Protocol(_))
            ),
            "{mode}"
        );
    }
}

#[test]
fn independent_watchdog_fires_while_the_supervisor_is_stalled_after_go() {
    let marker_directory = tempfile::tempdir().unwrap();
    let readiness = marker_directory.path().join("gated-marker-ready");
    let marker_due = marker_directory.path().join("final-marker-due");
    let marker = marker_directory.path().join("final-marker");
    let stall_gate = marker_directory.path().join("observation-stall-gate");
    let stall_reached = marker_directory.path().join("observation-stall-reached");
    let stall_release = marker_directory.path().join("observation-stall-release");
    let (runner, _dir) = fixture(
        "gated-marker",
        &[
            readiness.as_os_str().to_owned(),
            marker_due.as_os_str().to_owned(),
            marker.as_os_str().to_owned(),
        ],
        1,
    );
    let runner = runner.with_test_exit_observation_stall_until_release(
        stall_gate.clone(),
        stall_reached.clone(),
        stall_release.clone(),
        SUPERVISOR_GATE_TIMEOUT,
    );
    let started = Instant::now();
    let (pid, ready_pid, marker_before_release) = std::thread::scope(|scope| {
        let mut release = StallRelease::new(stall_release);
        let run = scope.spawn(|| runner.run(request("return 1;"), limits(400)));
        let (ready_pid, readiness_observed) =
            wait_for_gated_fixture_readiness(&readiness, Duration::from_secs(2));
        write_marker(&stall_gate, b"enter");
        wait_for_marker(&stall_reached, b"reached", Duration::from_secs(2));
        wait_until_after(
            readiness_observed,
            WATCHDOG_WALL_TIME + WATCHDOG_SETTLE_MARGIN,
        );
        write_marker(&marker_due, b"due");
        std::thread::sleep(FINAL_MARKER_MARGIN);
        let marker_before_release = read_optional_marker(&marker);
        assert!(
            !run.is_finished(),
            "supervisor escaped the observation gate before its explicit release"
        );
        release.release();
        let pid = timeout_pid(run.join().unwrap().unwrap_err());
        (pid, ready_pid, marker_before_release)
    });
    assert_eq!(
        pid, ready_pid,
        "readiness came from a different fixture PID"
    );
    // A schedule, not the property: the two assertions below are what say the
    // watchdog fired independently. This one only says the run did not hang,
    // and it is the first thing a loaded machine trips — so it reports what it
    // measured, and a failure here is read as "slow", not as "late watchdog".
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the gated run took {elapsed:?}; the watchdog's own behaviour is asserted below, so this bound failing on its own means the machine was slow rather than the watchdog late"
    );
    assert!(
        marker_before_release.is_none(),
        "fixture wrote its final marker while supervisor sealing was gated; watchdog termination did not stop it: {marker_before_release:?}"
    );
    assert!(
        !marker.exists(),
        "fixture final marker appeared after the supervisor gate was released"
    );
    assert_process_gone(pid);
}

#[test]
fn cancellation_latched_before_deadline_cannot_disarm_watchdog_before_seal() {
    let marker_directory = tempfile::tempdir().unwrap();
    let readiness = marker_directory.path().join("cancel-ready");
    let marker_due = marker_directory.path().join("cancel-final-marker-due");
    let final_marker = marker_directory.path().join("cancel-final-marker");
    let seal_reached = marker_directory.path().join("pre-seal-reached");
    let seal_release = marker_directory.path().join("pre-seal-release");
    let (runner, _dir) = fixture(
        "gated-marker",
        &[
            readiness.as_os_str().to_owned(),
            marker_due.as_os_str().to_owned(),
            final_marker.as_os_str().to_owned(),
        ],
        1,
    );
    let runner = runner.with_test_containment_seal_stall_until_release(
        seal_reached.clone(),
        seal_release.clone(),
        SUPERVISOR_GATE_TIMEOUT,
    );
    let cancelled = AtomicBool::new(false);
    let started = Instant::now();
    let (pid, ready_pid, marker_before_release) = std::thread::scope(|scope| {
        let mut release = StallRelease::new(seal_release);
        let run =
            scope.spawn(|| runner.run_with_cancel(request("return 1;"), limits(400), &cancelled));
        let (ready_pid, readiness_observed) =
            wait_for_gated_fixture_readiness(&readiness, Duration::from_secs(2));
        cancelled.store(true, Ordering::Release);
        wait_for_marker(&seal_reached, b"reached", Duration::from_secs(2));
        wait_until_after(
            readiness_observed,
            WATCHDOG_WALL_TIME + WATCHDOG_SETTLE_MARGIN,
        );
        write_marker(&marker_due, b"due");
        std::thread::sleep(FINAL_MARKER_MARGIN);
        let marker_before_release = read_optional_marker(&final_marker);
        assert!(
            !run.is_finished(),
            "supervisor escaped the pre-seal gate before its explicit release"
        );
        release.release();
        let pid = timeout_pid(run.join().unwrap().unwrap_err());
        (pid, ready_pid, marker_before_release)
    });
    assert_eq!(
        pid, ready_pid,
        "readiness came from a different fixture PID"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        marker_before_release.is_none(),
        "fixture wrote its final marker while cancellation cleanup was gated before seal; watchdog termination did not stop it: {marker_before_release:?}"
    );
    assert!(
        !final_marker.exists(),
        "fixture final marker appeared after cancellation cleanup was released"
    );
    assert_process_gone(pid);
}

#[test]
fn exit_crossing_deadline_is_timeout_and_claimed_watchdog_is_joined_before_return() {
    let marker_directory = tempfile::tempdir().unwrap();
    let phase_marker = marker_directory.path().join("delayed-success-phase");
    let (runner, _dir) = fixture(
        "delayed-success",
        &[OsString::from("80"), phase_marker.as_os_str().to_owned()],
        1,
    );
    // The child must reach its marker write before the stalled watchdog's
    // deferred kill lands. Its sleep is 80ms, but the margin has to absorb
    // exec startup on a loaded CI mac — where a first exec also pays
    // Gatekeeper's verification — so the post-claim stall is generous rather
    // than tight. Every ordering assertion below is unchanged by the width.
    let runner = runner
        .with_test_exit_observation_stall(Duration::from_millis(120))
        .with_test_watchdog_stall_after_firing_claim(Duration::from_millis(1200));
    let started = Instant::now();
    let pid = timeout_pid(runner.run(request("return 17;"), limits(50)).unwrap_err());
    assert!(
        started.elapsed() >= Duration::from_millis(1220),
        "returning before the post-claim stall completes would detach a live PGID capability"
    );
    assert!(started.elapsed() < Duration::from_secs(6));
    wait_for_marker(
        &phase_marker,
        b"delayed-success-phase",
        Duration::from_secs(2),
    );
    assert_process_gone(pid);
}

#[test]
fn partial_handoff_write_is_timed_out_killed_and_reaped() {
    let (runner, _dir) = fixture("partial-response-hang", &[], 1);
    let pid = timeout_pid(runner.run(request("return 1;"), limits(100)).unwrap_err());
    assert_process_gone(pid);
}

#[test]
fn stderr_is_discarded_instead_of_treated_as_a_capped_transport() {
    let (runner, _dir) = fixture("stderr-flood", &[], 1);
    let pid = timeout_pid(
        runner
            .run(
                request("return 1;"),
                Limits {
                    wall_time: Duration::from_millis(100),
                    stderr_bytes: 1,
                    ..Limits::default()
                },
            )
            .unwrap_err(),
    );
    assert_process_gone(pid);
}

#[cfg(any(unix, windows))]
#[test]
fn containment_kills_a_real_descendant() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("descendant.pid");
    let active_process_limit = descendant_active_process_limit();
    let (runner, _runner_dir) = fixture(
        "grandchild",
        &[pid_file.as_os_str().to_owned()],
        active_process_limit,
    );
    let root = timeout_pid(runner.run(request("return 1;"), limits(500)).unwrap_err());
    let descendant: u32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert_process_gone(root);
    assert_process_gone(descendant);
}

#[cfg(any(unix, windows))]
#[test]
fn natural_leader_exit_keeps_identity_until_descendants_are_sealed() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("natural-identities.pid");
    let (runner, _runner_dir) = fixture(
        "grandchild-natural-exit",
        &[pid_file.as_os_str().to_owned()],
        descendant_active_process_limit(),
    );
    let output = runner
        .run(request("return 23;"), Limits::default())
        .unwrap();
    assert_eq!(output.value, json!(23));
    let identities = std::fs::read_to_string(pid_file).unwrap();
    let mut identities = identities
        .lines()
        .map(|value| value.parse::<u32>().unwrap());
    let leader = identities.next().unwrap();
    let descendant = identities.next().unwrap();
    assert!(identities.next().is_none());
    assert_process_gone(leader);
    assert_process_gone(descendant);
}

#[cfg(windows)]
#[test]
fn job_cpu_limit_stops_a_cpu_bound_fixture_before_wall_deadline() {
    let (runner, _dir) = fixture("cpu-spin", &[], 1);
    let runner = runner.with_test_cpu_seconds(1);
    let started = Instant::now();
    let error = runner
        .run(
            request("return 1;"),
            Limits {
                wall_time: Duration::from_secs(10),
                ..Limits::default()
            },
        )
        .unwrap_err();
    assert!(matches!(error, RunnerError::ChildExit { .. }), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(9));
}

#[cfg(windows)]
fn descendant_active_process_limit() -> u32 {
    2
}

#[cfg(unix)]
fn descendant_active_process_limit() -> u32 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut limit) },
        0
    );
    let hard = u32::try_from(limit.rlim_max).unwrap_or(u32::MAX);
    assert!(
        hard >= 2,
        "RLIMIT_NPROC cannot admit the descendant fixture"
    );
    hard
}

#[cfg(windows)]
#[test]
fn job_memory_limit_stops_native_allocation() {
    let (runner, _dir) = fixture("allocate-native", &[], 1);
    let started = Instant::now();
    let error = runner
        .run(
            request("return 1;"),
            Limits {
                wall_time: Duration::from_secs(3),
                memory_bytes: 96 * 1024 * 1024,
                ..Limits::default()
            },
        )
        .unwrap_err();
    assert!(matches!(&error, RunnerError::ChildExit { .. }), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn response_capacity_is_enforced_in_handoff() {
    let (runner, _dir) = production();
    let error = runner
        .run(
            request("return 'x'.repeat(20_000);"),
            Limits {
                response_bytes: 1024,
                ..Limits::default()
            },
        )
        .unwrap_err();
    assert!(matches!(error, RunnerError::OutputTooLarge { limit: 1024 }));
}

#[cfg(windows)]
fn assert_process_gone(pid: u32) {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS,
                0,
                pid,
            )
        };
        if handle == 0 {
            let error = unsafe { GetLastError() };
            assert_eq!(
                error, ERROR_INVALID_PARAMETER,
                "failed to inspect PID {pid}: OS error {error}"
            );
            return;
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        unsafe { CloseHandle(handle) };
        if wait == WAIT_OBJECT_0 {
            return;
        }
        assert!(Instant::now() < deadline, "PID {pid} remains running");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn assert_process_gone(pid: u32) {
    let pid = i32::try_from(pid).expect("test PID fits pid_t");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let result = unsafe { libc::kill(pid, 0) };
        let error = std::io::Error::last_os_error().raw_os_error();
        if result == -1 && error == Some(libc::ESRCH) {
            return;
        }
        assert!(Instant::now() < deadline, "PID {pid} remains");
        std::thread::sleep(Duration::from_millis(10));
    }
}
