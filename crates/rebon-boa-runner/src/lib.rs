//! Process-isolated runner for restricted synchronous status-style scripts.
//!
//! The low-level request executes a plain function body. The status-line adapter in this crate
//! accepts the product's small synchronous module contract — a `default` export, or a named
//! `render` export — and runs it through the same hardened one-shot helper. Async functions,
//! Promise results, dynamic imports, and general ECMAScript modules remain deliberately
//! unsupported.

mod containment;
mod handoff;
mod protocol;

/// Reusable OS process boundary for one-shot restricted executors. This is the same primitive the
/// Boa helper uses; consumers still own their protocol, admission gate, supervision, and reaping.
pub use containment::Containment as ProcessContainment;

pub use protocol::StatusScriptRequest;

use containment::{Containment, WatchdogKill};
#[cfg(feature = "adversarial-fixtures")]
use handoff::FixtureCorruption;
use handoff::{ChildHandoff, ParentHandoff, MAX_REQUEST, MAX_RESPONSE};
use protocol::{ResponseBody, ResponseErrorKind, WireRequest, WireResponse, PROTOCOL_VERSION};
use serde_json::Value;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use thiserror::Error;

pub const DEFAULT_REQUEST_BYTES: usize = 64 * 1024;
pub const DEFAULT_RESPONSE_BYTES: usize = 8 * 1024;
pub const DEFAULT_STDERR_BYTES: usize = 0;
pub const DEFAULT_CODE_BYTES: usize = 32 * 1024;
pub const DEFAULT_PAYLOAD_BYTES: usize = 32 * 1024;
const MIN_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
/// Largest address-space/Job memory limit admitted by this restricted runner (1 GiB).
///
/// Keeping this finite prevents platform no-limit sentinels from silently disabling the hard
/// memory boundary, and it remains representable on supported 32-bit hosts.
pub const MAX_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Limits {
    /// Monotonic wall-clock execution deadline. It must be nonzero.
    pub wall_time: Duration,
    /// Per-process and process-tree memory cap, depending on the host OS. Values must be between
    /// 64 MiB and [`MAX_MEMORY_BYTES`] (1 GiB), inclusive.
    pub memory_bytes: u64,
    /// Logical request cap, bounded by the fixed 64-KiB handoff slot.
    pub request_bytes: usize,
    /// Logical response cap, bounded by the fixed 8-KiB handoff slot.
    ///
    /// The private handoff may reserve a few additional bytes for a typed overflow response;
    /// successful output is still checked against this exact value.
    pub response_bytes: usize,
    /// Ignored compatibility field. Child stderr is always connected to `Stdio::null()`, so this
    /// value is normalized to zero and never enables stderr capture.
    pub stderr_bytes: usize,
    /// Logical source-code cap, at most [`DEFAULT_CODE_BYTES`].
    pub code_bytes: usize,
    /// Logical serialized-JSON payload cap, at most [`DEFAULT_PAYLOAD_BYTES`].
    pub payload_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_time: Duration::from_millis(1_200),
            memory_bytes: 256 * 1024 * 1024,
            request_bytes: DEFAULT_REQUEST_BYTES,
            response_bytes: DEFAULT_RESPONSE_BYTES,
            stderr_bytes: DEFAULT_STDERR_BYTES,
            code_bytes: DEFAULT_CODE_BYTES,
            payload_bytes: DEFAULT_PAYLOAD_BYTES,
        }
    }
}

struct NormalizedLimits {
    public: Limits,
    cpu_seconds: u64,
    handoff_response_bytes: usize,
}

fn normalize_limits(mut limits: Limits) -> Result<NormalizedLimits, RunnerError> {
    #[cfg(not(any(unix, windows)))]
    return Err(RunnerError::ContainmentUnsupported(
        "process containment is available only on Unix and Windows".into(),
    ));

    if limits.wall_time.is_zero() {
        return Err(RunnerError::InvalidLimits("wall_time must be nonzero"));
    }
    if Instant::now().checked_add(limits.wall_time).is_none() {
        return Err(RunnerError::InvalidLimits(
            "wall_time is not representable by the monotonic clock",
        ));
    }
    if limits.memory_bytes < MIN_MEMORY_BYTES {
        return Err(RunnerError::InvalidLimits(
            "memory_bytes must be at least 64 MiB",
        ));
    }
    #[cfg(unix)]
    if limits.memory_bytes == libc::RLIM_INFINITY as u64 {
        return Err(RunnerError::InvalidLimits(
            "memory_bytes must not be the platform no-limit sentinel",
        ));
    }
    if limits.memory_bytes > MAX_MEMORY_BYTES {
        return Err(RunnerError::InvalidLimits(
            "memory_bytes exceeds the finite 1-GiB runner maximum",
        ));
    }
    if limits.request_bytes > MAX_REQUEST {
        return Err(RunnerError::InvalidLimits(
            "request_bytes exceeds the fixed handoff maximum",
        ));
    }
    if limits.response_bytes > MAX_RESPONSE {
        return Err(RunnerError::InvalidLimits(
            "response_bytes exceeds the fixed handoff maximum",
        ));
    }
    if limits.code_bytes > DEFAULT_CODE_BYTES {
        return Err(RunnerError::InvalidLimits(
            "code_bytes exceeds the helper maximum",
        ));
    }
    if limits.payload_bytes > DEFAULT_PAYLOAD_BYTES {
        return Err(RunnerError::InvalidLimits(
            "payload_bytes exceeds the helper maximum",
        ));
    }

    #[cfg(unix)]
    libc::rlim_t::try_from(limits.memory_bytes)
        .map_err(|_| RunnerError::InvalidLimits("memory_bytes is not representable by rlimit"))?;
    #[cfg(windows)]
    usize::try_from(limits.memory_bytes).map_err(|_| {
        RunnerError::InvalidLimits("memory_bytes is not representable by Windows Job Objects")
    })?;

    let cpu_seconds = limits
        .wall_time
        .as_secs()
        .checked_add(u64::from(limits.wall_time.subsec_nanos() != 0))
        .and_then(|seconds| seconds.checked_add(1))
        .ok_or(RunnerError::InvalidLimits(
            "wall_time is too large to derive a CPU backstop",
        ))?;
    #[cfg(unix)]
    libc::rlim_t::try_from(cpu_seconds).map_err(|_| {
        RunnerError::InvalidLimits("wall_time is not representable by the CPU rlimit")
    })?;
    #[cfg(windows)]
    cpu_seconds
        .checked_mul(10_000_000)
        .and_then(|ticks| i64::try_from(ticks).ok())
        .ok_or(RunnerError::InvalidLimits(
            "wall_time is not representable by Windows Job Objects",
        ))?;

    // stderr is never inherited or captured. Preserve the field for callers that already build
    // `Limits`, but normalize it so no internal path can accidentally treat it as a buffer cap.
    limits.stderr_bytes = 0;
    let handoff_response_bytes = limits
        .response_bytes
        .max(encoded_output_too_large_response().len());
    if handoff_response_bytes > MAX_RESPONSE {
        return Err(RunnerError::InvalidLimits(
            "the fixed response slot cannot hold its control response",
        ));
    }

    Ok(NormalizedLimits {
        public: limits,
        cpu_seconds,
        handoff_response_bytes,
    })
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("script exceeded its wall-clock deadline (reaped child {child_pid})")]
    Timeout { child_pid: u32 },
    #[error("script execution was cancelled before child start")]
    CancelledBeforeStart,
    #[error("script execution was cancelled (reaped child {child_pid})")]
    Cancelled { child_pid: u32 },
    #[error("request exceeds the configured {limit}-byte limit")]
    InputTooLarge { limit: usize },
    #[error("response exceeds the configured {limit}-byte limit")]
    OutputTooLarge { limit: usize },
    #[error("invalid child protocol: {0}")]
    Protocol(&'static str),
    #[error("child exited unsuccessfully: {status}")]
    ChildExit { status: ExitStatusText },
    #[error("invalid runner limits: {0}")]
    InvalidLimits(&'static str),
    #[error("containment is unsupported: {0}")]
    ContainmentUnsupported(String),
    #[error("failed to establish child containment: {0}")]
    ContainmentSetup(String),
    #[error("cleanup integrity failure after {primary}: {failures:?}")]
    CleanupIntegrity {
        primary: String,
        failures: Vec<&'static str>,
    },
    #[error("script failed: {message}")]
    Script { message: String },
    #[error("helper I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug)]
pub struct ExitStatusText(String);
impl fmt::Display for ExitStatusText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct StatusScriptOutput {
    pub value: Value,
    pub logs: Vec<String>,
    pub elapsed: Duration,
}

/// Maximum source file accepted by the status-line adapter. The generated wrapper remains below
/// the helper's fixed 32-KiB code ceiling.
pub const STATUS_LINE_SCRIPT_SOURCE_BYTES: usize = 28 * 1024;
pub const STATUS_LINE_SCRIPT_PAYLOAD_BYTES: usize = DEFAULT_PAYLOAD_BYTES;
pub const STATUS_LINE_SCRIPT_RESPONSE_BYTES: usize = DEFAULT_RESPONSE_BYTES;
pub const STATUS_LINE_SCRIPT_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
pub const STATUS_LINE_SCRIPT_MAX_LINES: usize = 5;
pub const STATUS_LINE_SCRIPT_TIMEOUT: Duration = Duration::from_millis(1_200);

/// The helper's product name, without the platform's executable suffix.
pub const HELPER_BINARY_STEM: &str = "rebon-boa-helper";

pub const fn helper_binary_name() -> &'static str {
    if cfg!(windows) {
        "rebon-boa-helper.exe"
    } else {
        "rebon-boa-helper"
    }
}

/// Deterministic product locations for the helper belonging to `executable`.
///
/// Native/npm installs put it beside the main executable. A macOS app bundle may put auxiliary
/// binaries under `Contents/Resources`, so that location is checked second. PATH and Cargo target
/// directories are intentionally not searched: accepting an unrelated helper would weaken the
/// product boundary and make packaged support depend on a developer checkout.
///
/// The rule itself lives in [`rebon_types::sibling_binary`].
pub fn helper_candidates_from_executable(executable: &Path) -> Vec<PathBuf> {
    rebon_types::sibling_binary::candidates(HELPER_BINARY_STEM, executable)
}

pub fn helper_path_from_executable(executable: &Path) -> Result<PathBuf, StatusLineScriptError> {
    let searched = helper_candidates_from_executable(executable);
    searched
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .ok_or(StatusLineScriptError::HelperNotFound { searched })
}

#[derive(Debug, Error)]
pub enum StatusLineScriptError {
    #[error("could not locate the bundled Boa helper (searched {searched:?})")]
    HelperNotFound { searched: Vec<PathBuf> },
    #[error("failed to locate the current executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[error("failed to read status script `{path}`: {source}")]
    ScriptIo { path: PathBuf, source: io::Error },
    #[error("status script exceeds the {limit}-byte source limit")]
    SourceTooLarge { limit: usize },
    #[error("invalid status script module: {0}")]
    InvalidModule(&'static str),
    #[error("status script returned an empty result")]
    Empty,
    #[error("status script returned a non-string result")]
    NonStringResult,
    #[error(transparent)]
    Runner(#[from] RunnerError),
}

#[derive(Debug, Clone)]
pub struct StatusLineScriptOutput {
    pub lines: Vec<String>,
    pub logs: Vec<String>,
    pub elapsed: Duration,
}

/// Product adapter for synchronous status-line render modules.
///
/// A fresh contained helper is created and reaped for every call. The adapter reads only the
/// explicitly configured script file in the parent process; user code receives a deeply frozen
/// JSON payload and a capture-only `console.log`, with no filesystem, network, environment, or
/// process primitives.
#[derive(Debug, Clone)]
pub struct StatusLineScriptRunner {
    helper: PathBuf,
}

impl StatusLineScriptRunner {
    pub fn new(helper: impl Into<PathBuf>) -> Result<Self, StatusLineScriptError> {
        let helper = helper.into();
        if !helper.is_absolute() {
            return Err(StatusLineScriptError::Runner(
                RunnerError::ContainmentSetup("helper path must be absolute".into()),
            ));
        }
        Ok(Self { helper })
    }

    pub fn from_current_executable() -> Result<Self, StatusLineScriptError> {
        let executable =
            std::env::current_exe().map_err(StatusLineScriptError::CurrentExecutable)?;
        Self::new(helper_path_from_executable(&executable)?)
    }

    pub fn run_file(
        &self,
        script: &Path,
        payload: Value,
        working_directory: &Path,
        columns: u16,
        lines: u16,
        cancelled: Option<&AtomicBool>,
    ) -> Result<StatusLineScriptOutput, StatusLineScriptError> {
        let working_directory =
            absolute_path(working_directory).map_err(|source| StatusLineScriptError::ScriptIo {
                path: working_directory.to_path_buf(),
                source,
            })?;
        let script = if script.is_absolute() {
            script.to_path_buf()
        } else {
            working_directory.join(script)
        };
        let source = fs::read(&script).map_err(|source| StatusLineScriptError::ScriptIo {
            path: script.clone(),
            source,
        })?;
        if source.len() > STATUS_LINE_SCRIPT_SOURCE_BYTES {
            return Err(StatusLineScriptError::SourceTooLarge {
                limit: STATUS_LINE_SCRIPT_SOURCE_BYTES,
            });
        }
        let source =
            std::str::from_utf8(&source).map_err(|error| StatusLineScriptError::ScriptIo {
                path: script,
                source: io::Error::new(io::ErrorKind::InvalidData, error),
            })?;
        self.run_source(
            source,
            payload,
            &working_directory,
            columns,
            lines,
            cancelled,
        )
    }

    pub fn run_source(
        &self,
        source: &str,
        mut payload: Value,
        working_directory: &Path,
        columns: u16,
        lines: u16,
        cancelled: Option<&AtomicBool>,
    ) -> Result<StatusLineScriptOutput, StatusLineScriptError> {
        if source.len() > STATUS_LINE_SCRIPT_SOURCE_BYTES {
            return Err(StatusLineScriptError::SourceTooLarge {
                limit: STATUS_LINE_SCRIPT_SOURCE_BYTES,
            });
        }
        let working_directory =
            absolute_path(working_directory).map_err(|source| StatusLineScriptError::ScriptIo {
                path: working_directory.to_path_buf(),
                source,
            })?;
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "terminal".into(),
                serde_json::json!({ "columns": columns, "lines": lines }),
            );
        }
        let request = StatusScriptRequest {
            code: wrap_status_line_module(source)?,
            payload,
        };
        let limits = Limits {
            wall_time: STATUS_LINE_SCRIPT_TIMEOUT,
            memory_bytes: STATUS_LINE_SCRIPT_MEMORY_BYTES,
            request_bytes: DEFAULT_REQUEST_BYTES,
            response_bytes: STATUS_LINE_SCRIPT_RESPONSE_BYTES,
            stderr_bytes: DEFAULT_STDERR_BYTES,
            code_bytes: DEFAULT_CODE_BYTES,
            payload_bytes: STATUS_LINE_SCRIPT_PAYLOAD_BYTES,
        };
        let runner = IsolatedJsRunner::new(self.helper.clone(), working_directory)?;
        let output = match cancelled {
            Some(cancelled) => runner.run_with_cancel(request, limits, cancelled)?,
            None => runner.run(request, limits)?,
        };
        let text = output
            .value
            .as_str()
            .ok_or(StatusLineScriptError::NonStringResult)?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.trim().is_empty() {
            return Err(StatusLineScriptError::Empty);
        }
        Ok(StatusLineScriptOutput {
            lines: text
                .lines()
                .take(STATUS_LINE_SCRIPT_MAX_LINES)
                .map(str::to_string)
                .collect(),
            logs: output.logs,
            elapsed: output.elapsed,
        })
    }
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir().map(|current| current.join(path))
    }
}

fn wrap_status_line_module(source: &str) -> Result<String, StatusLineScriptError> {
    const DEFAULT_EXPORT: &str = "export default";
    const NAMED_EXPORT: &str = "export function render";
    const BINDING: &str = "__rebon_status_render_7d5b";

    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let transformed = if let Some(index) = source.rfind(DEFAULT_EXPORT) {
        let mut transformed = String::with_capacity(source.len() + 256);
        transformed.push_str(&source[..index]);
        transformed.push_str("const ");
        transformed.push_str(BINDING);
        transformed.push_str(" =");
        transformed.push_str(&source[index + DEFAULT_EXPORT.len()..]);
        transformed
    } else if let Some(index) = source.rfind(NAMED_EXPORT) {
        let mut transformed = String::with_capacity(source.len() + 256);
        transformed.push_str(&source[..index]);
        transformed.push_str("function ");
        transformed.push_str(BINDING);
        transformed.push_str(&source[index + NAMED_EXPORT.len()..]);
        transformed
    } else {
        return Err(StatusLineScriptError::InvalidModule(
            "expected a synchronous `export default` or `export function render`",
        ));
    };

    Ok(format!(
        "{transformed}\n\nif (typeof {BINDING} !== 'function') {{ throw new TypeError('status script export must be a function'); }}\nconst __rebon_status_value_7d5b = {BINDING}(payload);\nreturn __rebon_status_value_7d5b;"
    ))
}

enum SupervisorObservation {
    Running,
    /// Unix deliberately reports no status here: `waitid(..., WNOWAIT)` observes exit without
    /// reaping the process-group leader. Windows may cache the status because Job handles are a
    /// stable kernel identity.
    Exited(Option<ExitStatus>),
}

struct WatchdogThreadOutcome {
    deadline_won: bool,
    termination: io::Result<()>,
}

struct DeadlineWatchdog {
    stop: Option<Sender<()>>,
    deadline: Instant,
    deadline_claimed: Arc<AtomicBool>,
    handle: Option<JoinHandle<WatchdogThreadOutcome>>,
}

impl DeadlineWatchdog {
    fn arm(
        mut kill: WatchdogKill,
        deadline: Instant,
        #[cfg(feature = "adversarial-fixtures")] stall_after_firing_claim: Option<TestStallHook>,
        #[cfg(not(feature = "adversarial-fixtures"))] stall_after_firing_claim: Option<Duration>,
    ) -> io::Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let deadline_claimed = Arc::new(AtomicBool::new(false));
        let thread_deadline_claimed = Arc::clone(&deadline_claimed);
        let handle = thread::Builder::new()
            .name("rebon-boa-deadline".into())
            .spawn(move || {
                let mut stop_disconnected = false;
                loop {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    if stop_disconnected {
                        thread::sleep(remaining);
                        continue;
                    }
                    match receiver.recv_timeout(remaining) {
                        Ok(()) if Instant::now() < deadline => {
                            // The watchdog thread is the sole deadline arbiter. Its clock check is
                            // the linearization point for accepting a non-timeout outcome.
                            return WatchdogThreadOutcome {
                                deadline_won: false,
                                termination: Ok(()),
                            };
                        }
                        Ok(()) | Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => stop_disconnected = true,
                    }
                }

                thread_deadline_claimed.store(true, Ordering::Release);
                // Fault injection is intentionally after the thread has decided the deadline won
                // while it still owns the numeric Unix PGID kill capability. The parent must join
                // and retain the unreaped leader throughout this pause.
                #[cfg(feature = "adversarial-fixtures")]
                if let Some(stall) = stall_after_firing_claim {
                    // Past the deadline by definition, so the hook's own
                    // cutoff has to sit beyond it or the stall would be
                    // skipped outright.
                    let latest_start = Instant::now() + stall.duration;
                    run_test_stall_hook(stall, latest_start);
                }
                #[cfg(not(feature = "adversarial-fixtures"))]
                if let Some(stall) = stall_after_firing_claim {
                    thread::sleep(stall);
                }
                WatchdogThreadOutcome {
                    deadline_won: true,
                    termination: kill.terminate(),
                }
            })?;
        Ok(Self {
            stop: Some(stop),
            deadline,
            deadline_claimed,
            handle: Some(handle),
        })
    }

    fn deadline_is_due(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// A watchdog holding a Unix numeric PGID capability is never detached. The only watchdog
    /// operation after the deadline decision is an optional finite test pause followed by one
    /// finite kernel termination call, so joining is safe and keeps the unreaped leader owned
    /// until targeting is irrevocably finished.
    fn shutdown(mut self, request_stop: bool) -> (bool, io::Result<()>) {
        if request_stop {
            if let Some(stop) = &self.stop {
                let _ = stop.send(());
            }
        }
        self.stop.take();
        let Some(handle) = self.handle.take() else {
            return (
                self.deadline_claimed.load(Ordering::Acquire),
                Err(io::Error::other("watchdog handle is absent")),
            );
        };
        match handle.join() {
            Ok(outcome) => (outcome.deadline_won, outcome.termination),
            Err(_) => (
                self.deadline_claimed.load(Ordering::Acquire),
                Err(io::Error::other("deadline watchdog panicked")),
            ),
        }
    }
}

fn seal_containment_and_finish_watchdog(
    containment: &mut Containment,
    watchdog: DeadlineWatchdog,
    direct_child_exited: bool,
    allow_early_stop: bool,
    failures: &mut Vec<&'static str>,
) -> bool {
    // GO may already be visible. Keep the independent deadline kill capability armed until the
    // supervisor has synchronously terminated/sealed the containment unit. If sealing fails, do
    // not let the supervisor disarm the watchdog: joining it must wait for its deadline attempt.
    let sealed = containment.terminate_and_seal(direct_child_exited).is_ok();
    if !sealed {
        failures.push("containment terminate/query/close");
    }
    let (deadline_won, watchdog_result) = watchdog.shutdown(sealed && allow_early_stop);
    if watchdog_result.is_err() {
        failures.push("deadline watchdog cancellation/join");
    }
    deadline_won
}

#[cfg(feature = "adversarial-fixtures")]
#[derive(Debug, Clone)]
struct TestStallHook {
    duration: Duration,
    gate: Option<PathBuf>,
    reached: Option<PathBuf>,
    release: Option<PathBuf>,
}

#[cfg(feature = "adversarial-fixtures")]
fn run_test_stall_hook(hook: TestStallHook, latest_start: Instant) {
    if let Some(gate) = hook.gate {
        while !gate.exists() {
            if Instant::now() >= latest_start {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    if Instant::now() >= latest_start {
        return;
    }
    if let Some(reached) = hook.reached {
        let _ = write_test_marker(&reached, b"reached");
    }
    if let Some(release) = hook.release {
        let release_deadline = Instant::now() + hook.duration;
        while !release.exists() {
            if Instant::now() >= release_deadline {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    } else {
        thread::sleep(hook.duration);
    }
}

#[derive(Debug, Clone)]
pub struct IsolatedJsRunner {
    helper: PathBuf,
    working_directory: PathBuf,
    helper_args: Vec<std::ffi::OsString>,
    active_process_limit: u32,
    #[cfg(feature = "adversarial-fixtures")]
    supervisor_stall_in_exit_observation: Option<TestStallHook>,
    #[cfg(feature = "adversarial-fixtures")]
    supervisor_stall_before_containment_seal: Option<TestStallHook>,
    #[cfg(feature = "adversarial-fixtures")]
    watchdog_stall_after_firing_claim: Option<TestStallHook>,
    #[cfg(feature = "adversarial-fixtures")]
    cpu_seconds_override: Option<u64>,
}

impl IsolatedJsRunner {
    pub fn new(
        helper: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
    ) -> Result<Self, RunnerError> {
        let helper = helper.into();
        let working_directory = working_directory.into();
        if !helper.is_absolute() || !working_directory.is_absolute() {
            return Err(RunnerError::ContainmentSetup(
                "helper and working directory must be absolute paths".into(),
            ));
        }
        Ok(Self {
            helper,
            working_directory,
            helper_args: Vec::new(),
            active_process_limit: 1,
            #[cfg(feature = "adversarial-fixtures")]
            supervisor_stall_in_exit_observation: None,
            #[cfg(feature = "adversarial-fixtures")]
            supervisor_stall_before_containment_seal: None,
            #[cfg(feature = "adversarial-fixtures")]
            watchdog_stall_after_firing_claim: None,
            #[cfg(feature = "adversarial-fixtures")]
            cpu_seconds_override: None,
        })
    }

    #[cfg(feature = "adversarial-fixtures")]
    #[doc(hidden)]
    pub fn new_test_fixture(
        executable: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
        args: Vec<std::ffi::OsString>,
        active_process_limit: u32,
    ) -> Result<Self, RunnerError> {
        if active_process_limit == 0 {
            return Err(RunnerError::InvalidLimits(
                "active process limit must be nonzero",
            ));
        }
        let mut runner = Self::new(executable, working_directory)?;
        runner.helper_args = args;
        runner.active_process_limit = active_process_limit;
        Ok(runner)
    }

    #[cfg(feature = "adversarial-fixtures")]
    #[doc(hidden)]
    pub fn with_test_exit_observation_stall(mut self, duration: Duration) -> Self {
        self.supervisor_stall_in_exit_observation = Some(TestStallHook {
            duration,
            gate: None,
            reached: None,
            release: None,
        });
        self
    }

    #[cfg(feature = "adversarial-fixtures")]
    #[doc(hidden)]
    pub fn with_test_exit_observation_stall_until_release(
        mut self,
        gate: impl Into<PathBuf>,
        reached: impl Into<PathBuf>,
        release: impl Into<PathBuf>,
        maximum_duration: Duration,
    ) -> Self {
        self.supervisor_stall_in_exit_observation = Some(TestStallHook {
            duration: maximum_duration,
            gate: Some(gate.into()),
            reached: Some(reached.into()),
            release: Some(release.into()),
        });
        self
    }

    #[cfg(feature = "adversarial-fixtures")]
    #[doc(hidden)]
    pub fn with_test_containment_seal_stall_until_release(
        mut self,
        reached: impl Into<PathBuf>,
        release: impl Into<PathBuf>,
        maximum_duration: Duration,
    ) -> Self {
        self.supervisor_stall_before_containment_seal = Some(TestStallHook {
            duration: maximum_duration,
            gate: None,
            reached: Some(reached.into()),
            release: Some(release.into()),
        });
        self
    }

    #[cfg(feature = "adversarial-fixtures")]
    #[doc(hidden)]
    pub fn with_test_watchdog_stall_after_firing_claim(mut self, duration: Duration) -> Self {
        self.watchdog_stall_after_firing_claim = Some(TestStallHook {
            duration,
            gate: None,
            reached: None,
            release: None,
        });
        self
    }

    /// Stall the watchdog after it claims the kill until the test releases it,
    /// rather than for a fixed span. A duration has to be guessed wide enough
    /// to cover process startup on a loaded machine, which makes the test a
    /// race it can lose; waiting for a file the test writes makes the ordering
    /// explicit instead. `maximum_duration` is only a backstop against a test
    /// that never releases.
    #[cfg(feature = "adversarial-fixtures")]
    #[doc(hidden)]
    pub fn with_test_watchdog_stall_after_firing_claim_until_release(
        mut self,
        reached: impl Into<PathBuf>,
        release: impl Into<PathBuf>,
        maximum_duration: Duration,
    ) -> Self {
        self.watchdog_stall_after_firing_claim = Some(TestStallHook {
            duration: maximum_duration,
            gate: None,
            reached: Some(reached.into()),
            release: Some(release.into()),
        });
        self
    }

    #[cfg(feature = "adversarial-fixtures")]
    #[doc(hidden)]
    pub fn with_test_cpu_seconds(mut self, cpu_seconds: u64) -> Self {
        assert!(cpu_seconds > 0, "test CPU limit must be nonzero");
        self.cpu_seconds_override = Some(cpu_seconds);
        self
    }

    pub fn run(
        &self,
        request: StatusScriptRequest,
        limits: Limits,
    ) -> Result<StatusScriptOutput, RunnerError> {
        self.run_inner(request, limits, None)
    }

    pub fn run_with_cancel(
        &self,
        request: StatusScriptRequest,
        limits: Limits,
        cancelled: &AtomicBool,
    ) -> Result<StatusScriptOutput, RunnerError> {
        self.run_inner(request, limits, Some(cancelled))
    }

    fn run_inner(
        &self,
        request: StatusScriptRequest,
        limits: Limits,
        cancelled: Option<&AtomicBool>,
    ) -> Result<StatusScriptOutput, RunnerError> {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(RunnerError::CancelledBeforeStart);
        }
        let normalized = normalize_limits(limits)?;
        #[cfg(feature = "adversarial-fixtures")]
        let cpu_seconds = self.cpu_seconds_override.unwrap_or(normalized.cpu_seconds);
        #[cfg(not(feature = "adversarial-fixtures"))]
        let cpu_seconds = normalized.cpu_seconds;
        let handoff_response_bytes = normalized.handoff_response_bytes;
        let limits = normalized.public;
        validate_request(&request, &limits)?;
        let encoded = serde_json::to_vec(&WireRequest {
            protocol_version: PROTOCOL_VERSION,
            request,
        })
        .map_err(|_| RunnerError::Protocol("request is not serializable"))?;
        if encoded.len() > limits.request_bytes {
            return Err(RunnerError::InputTooLarge {
                limit: limits.request_bytes,
            });
        }
        let handoff =
            ParentHandoff::create(&encoded, limits.request_bytes, handoff_response_bytes)?;
        let child_file = handoff.child_file()?;
        let mut command = Command::new(&self.helper);
        command
            .args(&self.helper_args)
            .current_dir(&self.working_directory)
            .env_clear()
            .stdin(Stdio::from(child_file))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Containment::configure_command(
            &mut command,
            limits.memory_bytes,
            cpu_seconds,
            self.active_process_limit,
        )
        .map_err(|error| RunnerError::ContainmentSetup(error.to_string()))?;
        let mut child = command.spawn()?;
        let child_pid = child.id();
        let mut containment = match Containment::establish(
            &mut child,
            limits.memory_bytes,
            cpu_seconds,
            self.active_process_limit,
        ) {
            Ok(value) => value,
            Err(error) => {
                let primary = RunnerError::ContainmentSetup(error.to_string());
                return finish_failed_start(primary, &mut child, None, Vec::new());
            }
        };
        let started = Instant::now();
        let Some(deadline) = started.checked_add(limits.wall_time) else {
            return finish_failed_start(
                RunnerError::InvalidLimits("wall_time is not representable by the monotonic clock"),
                &mut child,
                Some(&mut containment),
                Vec::new(),
            );
        };
        let watchdog_kill = match containment.watchdog_kill() {
            Ok(value) => value,
            Err(error) => {
                return finish_failed_start(
                    RunnerError::ContainmentSetup(format!(
                        "failed to create deadline watchdog handle: {error}"
                    )),
                    &mut child,
                    Some(&mut containment),
                    Vec::new(),
                );
            }
        };
        #[cfg(feature = "adversarial-fixtures")]
        let watchdog_stall = self.watchdog_stall_after_firing_claim.clone();
        #[cfg(not(feature = "adversarial-fixtures"))]
        let watchdog_stall = None;
        let watchdog = match DeadlineWatchdog::arm(watchdog_kill, deadline, watchdog_stall) {
            Ok(value) => value,
            Err(error) => {
                return finish_failed_start(
                    RunnerError::ContainmentSetup(format!(
                        "failed to arm deadline watchdog: {error}"
                    )),
                    &mut child,
                    Some(&mut containment),
                    Vec::new(),
                );
            }
        };
        if let Err(error) = handoff.admit() {
            let mut failures = Vec::new();
            let deadline_won = seal_containment_and_finish_watchdog(
                &mut containment,
                watchdog,
                false,
                true,
                &mut failures,
            );
            let primary = if deadline_won {
                RunnerError::Timeout { child_pid }
            } else {
                RunnerError::Io(error)
            };
            return finish_failed_start(primary, &mut child, Some(&mut containment), failures);
        }

        #[cfg(feature = "adversarial-fixtures")]
        let mut exit_observation_stall = self.supervisor_stall_in_exit_observation.clone();

        let mut outcome = loop {
            // Deadline always outranks cancellation and child success. There is deliberately no
            // watchdog state transition around the following exit observation.
            if watchdog.deadline_is_due() {
                break Err(RunnerError::Timeout { child_pid });
            }
            if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                break Err(RunnerError::Cancelled { child_pid });
            }

            // This hook fires after the supervisor commits to the exit-observation path. A stall
            // here models a page fault or blocked status primitive while the watchdog stays armed.
            #[cfg(feature = "adversarial-fixtures")]
            if let Some(stall) = exit_observation_stall.take() {
                run_test_stall_hook(stall, watchdog.deadline);
            }

            let observation = match observe_direct_child_exit(&mut child) {
                Ok(observation) => observation,
                Err(error) => break Err(RunnerError::Io(error)),
            };
            match observation {
                SupervisorObservation::Exited(status) => break Ok(status),
                SupervisorObservation::Running => thread::sleep(Duration::from_millis(2)),
            }
        };

        let timeout_outcome = matches!(&outcome, Err(RunnerError::Timeout { .. }));
        let natural = outcome.is_ok();
        let mut failures = Vec::new();
        // This hook is deliberately after a provisional outcome is latched and immediately before
        // supervisor sealing. The watchdog remains armed throughout the injected pause.
        #[cfg(feature = "adversarial-fixtures")]
        if let Some(stall) = self.supervisor_stall_before_containment_seal.clone() {
            run_test_stall_hook(stall, watchdog.deadline);
        }
        let deadline_won = seal_containment_and_finish_watchdog(
            &mut containment,
            watchdog,
            natural,
            !timeout_outcome,
            &mut failures,
        );
        if deadline_won && !timeout_outcome {
            outcome = Err(RunnerError::Timeout { child_pid });
        }
        let reaped = match outcome {
            Ok(Some(status)) => Some(status),
            Ok(None) => match child.wait() {
                Ok(status) => Some(status),
                Err(_) => {
                    failures.push("direct-child wait after containment seal");
                    None
                }
            },
            Err(primary) => match reap_child_bounded(&mut child, Duration::from_secs(2)) {
                Ok(Some(_)) => {
                    if failures.is_empty() {
                        return Err(primary);
                    }
                    return Err(RunnerError::CleanupIntegrity {
                        primary: primary.to_string(),
                        failures,
                    });
                }
                Ok(None) => {
                    failures.push("bounded direct-child reap");
                    None
                }
                Err(_) => {
                    failures.push("direct-child try_wait");
                    None
                }
            },
        };
        if !failures.is_empty() {
            return Err(RunnerError::CleanupIntegrity {
                primary: "natural child exit".into(),
                failures,
            });
        }
        let status = reaped.expect("natural outcome carries status");
        if !status.success() {
            return Err(RunnerError::ChildExit {
                status: ExitStatusText(status.to_string()),
            });
        }
        let response_bytes = handoff.read_response().map_err(map_handoff_read)?;
        if response_bytes.len() > limits.response_bytes {
            return Err(RunnerError::OutputTooLarge {
                limit: limits.response_bytes,
            });
        }
        let response: WireResponse = serde_json::from_slice(&response_bytes)
            .map_err(|_| RunnerError::Protocol("response is not one strict JSON value"))?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(RunnerError::Protocol("response version mismatch"));
        }
        match response.body {
            ResponseBody::Ok { value, logs } => Ok(StatusScriptOutput {
                value,
                logs,
                elapsed: started.elapsed(),
            }),
            ResponseBody::Error {
                kind: ResponseErrorKind::OutputTooLarge,
                ..
            } => Err(RunnerError::OutputTooLarge {
                limit: limits.response_bytes,
            }),
            ResponseBody::Error {
                kind: ResponseErrorKind::Script,
                message,
            } => Err(RunnerError::Script {
                message: bound_text(&message, 512),
            }),
        }
    }
}

fn finish_failed_start(
    primary: RunnerError,
    child: &mut std::process::Child,
    containment: Option<&mut Containment>,
    mut failures: Vec<&'static str>,
) -> Result<StatusScriptOutput, RunnerError> {
    if let Some(containment) = containment {
        if containment.terminate_and_seal(false).is_err() {
            failures.push("containment terminate/query/close");
        }
    } else if child.kill().is_err() {
        failures.push("direct child kill");
    }
    match reap_child_bounded(child, Duration::from_secs(2)) {
        Ok(Some(_)) => {}
        Ok(None) => failures.push("bounded direct-child reap"),
        Err(_) => failures.push("direct-child try_wait"),
    }
    if failures.is_empty() {
        Err(primary)
    } else {
        Err(RunnerError::CleanupIntegrity {
            primary: primary.to_string(),
            failures,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitObservation {
    Running,
    Exited,
}

/// Observe whether an owned direct child exited without reaping its Unix process-group leader.
/// The caller must seal the containment unit before the final `Child::wait`.
pub fn observe_child_exit(child: &mut std::process::Child) -> io::Result<ChildExitObservation> {
    observe_direct_child_exit(child).map(|observation| match observation {
        SupervisorObservation::Running => ChildExitObservation::Running,
        SupervisorObservation::Exited(_) => ChildExitObservation::Exited,
    })
}

#[cfg(windows)]
fn observe_direct_child_exit(child: &mut std::process::Child) -> io::Result<SupervisorObservation> {
    child.try_wait().map(|status| match status {
        Some(status) => SupervisorObservation::Exited(Some(status)),
        None => SupervisorObservation::Running,
    })
}

#[cfg(unix)]
fn observe_direct_child_exit(child: &mut std::process::Child) -> io::Result<SupervisorObservation> {
    let id = libc::id_t::try_from(child.id())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child PID is not an id_t"))?;
    // SAFETY: `waitid` initializes `info`; P_PID targets this owned direct child; WNOWAIT leaves
    // the exited leader waitable so its PGID cannot be reused before watchdog join + containment
    // seal. A zero si_pid with WNOHANG means there is no state change to report.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `info` was initialized by a successful waitid call.
    let observed_pid = unsafe { info.si_pid() };
    if observed_pid == 0 {
        Ok(SupervisorObservation::Running)
    } else if u32::try_from(observed_pid).ok() == Some(child.id()) {
        Ok(SupervisorObservation::Exited(None))
    } else {
        Err(io::Error::other("waitid reported an unexpected child"))
    }
}

#[cfg(not(any(unix, windows)))]
fn observe_direct_child_exit(
    _child: &mut std::process::Child,
) -> io::Result<SupervisorObservation> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "direct child observation is unsupported",
    ))
}

pub fn reap_child_bounded(
    child: &mut std::process::Child,
    timeout: Duration,
) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(Some(status)),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            None => return Ok(None),
        }
    }
}

fn map_handoff_read(error: io::Error) -> RunnerError {
    match error.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
            RunnerError::Protocol("invalid or incomplete handoff response")
        }
        _ => RunnerError::Io(error),
    }
}

fn validate_request(request: &StatusScriptRequest, limits: &Limits) -> Result<(), RunnerError> {
    if request.code.len() > limits.code_bytes {
        return Err(RunnerError::InputTooLarge {
            limit: limits.code_bytes,
        });
    }
    let payload_len = serde_json::to_vec(&request.payload)
        .map_err(|_| RunnerError::Protocol("payload is not serializable"))?
        .len();
    if payload_len > limits.payload_bytes {
        return Err(RunnerError::InputTooLarge {
            limit: limits.payload_bytes,
        });
    }
    Ok(())
}

fn bound_text(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
pub fn helper_path_is_absolute(path: &Path) -> bool {
    path.is_absolute()
}

#[doc(hidden)]
pub fn run_helper() -> i32 {
    let handoff = match ChildHandoff::open_stdin() {
        Ok(value) => value,
        Err(_) => return 74,
    };
    let response = match helper_once(&handoff) {
        Ok(response) => response,
        Err(message) => WireResponse {
            protocol_version: PROTOCOL_VERSION,
            body: ResponseBody::Error {
                kind: ResponseErrorKind::Script,
                message: bound_text(&message, 512),
            },
        },
    };
    write_helper_response(&handoff, &response).map_or(74, |()| 0)
}

#[derive(boa_engine::Trace, boa_engine::Finalize, boa_engine::JsData)]
struct HelperLogState {
    stringify: boa_engine::object::builtins::JsFunction,
    #[unsafe_ignore_trace]
    logs: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

fn helper_js_error(error: boa_engine::JsError, context: &mut boa_engine::Context) -> String {
    let message = error
        .to_opaque(context)
        .to_string(context)
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_else(|_| "JavaScript execution failed".into());
    bound_text(&message, 512)
}

fn helper_type_error(message: impl Into<String>) -> boa_engine::JsError {
    boa_engine::JsNativeError::typ()
        .with_message(message.into())
        .into()
}

fn native_console_log(
    _this: &boa_engine::JsValue,
    args: &[boa_engine::JsValue],
    context: &mut boa_engine::Context,
) -> boa_engine::JsResult<boa_engine::JsValue> {
    let (logs, stringify) = context
        .get_data::<HelperLogState>()
        .map(|state| (state.logs.clone(), state.stringify.clone()))
        .ok_or_else(|| helper_type_error("console host state is unavailable"))?;

    let mut parts = Vec::with_capacity(args.len());
    for value in args {
        if let Some(string) = value.as_string() {
            parts.push(string.to_std_string_escaped());
        } else if value.is_undefined() {
            // An argument that `is_undefined()` contributes an empty entry to the
            // space-joined line, matching what stringifying it would yield.
            parts.push(String::new());
        } else {
            let encoded = stringify.call(
                &boa_engine::JsValue::undefined(),
                std::slice::from_ref(value),
                context,
            )?;
            if encoded.is_undefined() {
                parts.push(String::new());
            } else {
                let encoded = encoded.as_string().ok_or_else(|| {
                    helper_type_error("host JSON serializer returned a non-string")
                })?;
                parts.push(encoded.to_std_string_escaped());
            }
        }
    }
    let line = parts.join(" ");
    if line.chars().count() > 512 {
        return Err(helper_type_error("log entry limit exceeded"));
    }
    let mut logs = logs
        .try_borrow_mut()
        .map_err(|_| helper_type_error("recursive console log borrow"))?;
    if logs.len() >= 32 {
        return Err(helper_type_error("log limit exceeded"));
    }
    logs.push(line);
    Ok(boa_engine::JsValue::undefined())
}

fn helper_once(handoff: &ChildHandoff) -> Result<WireResponse, String> {
    use boa_engine::{
        js_string,
        object::{
            builtins::{JsFunction, JsPromise},
            ObjectInitializer,
        },
        Context, JsValue, NativeFunction, Source,
    };
    let body = handoff.read_request().map_err(|error| error.to_string())?;
    let wire: WireRequest =
        serde_json::from_slice(&body).map_err(|_| "request is not strict JSON".to_string())?;
    if wire.protocol_version != PROTOCOL_VERSION {
        return Err("request version mismatch".into());
    }
    if wire.request.code.len() > DEFAULT_CODE_BYTES {
        return Err("code exceeds helper limit".into());
    }
    let payload_json =
        serde_json::to_vec(&wire.request.payload).map_err(|_| "payload is not JSON".to_string())?;
    if payload_json.len() > DEFAULT_PAYLOAD_BYTES {
        return Err("payload exceeds helper limit".into());
    }

    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(1_000_000);
    context.runtime_limits_mut().set_recursion_limit(256);
    context.runtime_limits_mut().set_stack_size_limit(512);

    let logs = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    // Capture the pristine JSON.stringify function before user code runs. Both result and log
    // serialization use this private closure, so replacing globalThis.JSON cannot affect framing
    // or cap enforcement and cycles retain standard JSON errors.
    let stringify = context
        .eval(Source::from_bytes(
            "(function (stringify) { return function (value) { return stringify(value); }; })(JSON.stringify)",
        ))
        .map_err(|error| helper_js_error(error, &mut context))?
        .as_object()
        .cloned()
        .and_then(JsFunction::from_object)
        .ok_or_else(|| "host JSON serializer is not callable".to_string())?;
    context.insert_data(HelperLogState {
        stringify: stringify.clone(),
        logs: logs.clone(),
    });
    let console = ObjectInitializer::new(&mut context)
        .function(
            NativeFunction::from_fn_ptr(native_console_log),
            js_string!("log"),
            0,
        )
        .build();

    // The fixed freezer is evaluated before any user code and retained only as a Rust handle. It
    // recursively freezes the JSON payload and the explicit console binding without introducing
    // a user-visible host-state lexical.
    let freezer = context
        .eval(Source::from_bytes(
            "(function deepFreeze(value) { if (value !== null && (typeof value === 'object' || typeof value === 'function')) { Object.freeze(value); for (const key of Object.keys(value)) deepFreeze(value[key]); } return value; })",
        ))
        .map_err(|error| helper_js_error(error, &mut context))?
        .as_object()
        .cloned()
        .and_then(JsFunction::from_object)
        .ok_or_else(|| "payload freezer is not callable".to_string())?;
    let payload = JsValue::from_json(&wire.request.payload, &mut context)
        .map_err(|error| helper_js_error(error, &mut context))?;
    let payload = freezer
        .call(&JsValue::undefined(), &[payload], &mut context)
        .map_err(|error| helper_js_error(error, &mut context))?;
    let console = freezer
        .call(&JsValue::undefined(), &[console.into()], &mut context)
        .map_err(|error| helper_js_error(error, &mut context))?;

    // Function() parses the JSON-quoted source as a real dynamic-function body. Consequently the
    // user body has a global outer environment, not a generated wrapper lexical, and its only
    // runner-provided bindings are the explicit payload and console parameters.
    let function_body = format!("\"use strict\";\n{}", wire.request.code);
    let function_body_literal =
        serde_json::to_string(&function_body).map_err(|_| "code encoding failed".to_string())?;
    let function_source = format!("Function('payload', 'console', {function_body_literal})");
    let function = context
        .eval(Source::from_bytes(&function_source))
        .map_err(|error| helper_js_error(error, &mut context))?
        .as_object()
        .cloned()
        .and_then(JsFunction::from_object)
        .ok_or_else(|| "user body did not compile to a function".to_string())?;
    let value = function
        .call(&JsValue::undefined(), &[payload, console], &mut context)
        .map_err(|error| helper_js_error(error, &mut context))?;

    // Brand-check against Boa's internal Promise object rather than the mutable global Promise
    // constructor or instanceof semantics.
    if value
        .as_object()
        .cloned()
        .and_then(|object| JsPromise::from_object(object).ok())
        .is_some()
    {
        return Err(
            "Promise results are not supported; this runner only executes synchronous scripts"
                .into(),
        );
    }
    if value.is_undefined() {
        return Err("result is not JSON-compatible".into());
    }
    let encoded = stringify
        .call(&JsValue::undefined(), &[value], &mut context)
        .map_err(|error| helper_js_error(error, &mut context))?;
    let encoded = encoded
        .as_string()
        .ok_or_else(|| "result is not JSON-compatible".to_string())?
        .to_std_string_escaped();
    let value = serde_json::from_str(&encoded)
        .map_err(|_| "host JSON serializer returned invalid JSON".to_string())?;
    let logs = logs
        .try_borrow()
        .map_err(|_| "console host state is borrowed".to_string())?
        .clone();
    Ok(WireResponse {
        protocol_version: PROTOCOL_VERSION,
        body: ResponseBody::Ok { value, logs },
    })
}

fn output_too_large_response() -> WireResponse {
    WireResponse {
        protocol_version: PROTOCOL_VERSION,
        body: ResponseBody::Error {
            kind: ResponseErrorKind::OutputTooLarge,
            message: "response exceeds helper limit".into(),
        },
    }
}

fn encoded_output_too_large_response() -> Vec<u8> {
    serde_json::to_vec(&output_too_large_response())
        .expect("the fixed overflow response is always serializable")
}

fn write_helper_response(handoff: &ChildHandoff, response: &WireResponse) -> io::Result<()> {
    let encoded = serde_json::to_vec(response).map_err(io::Error::other)?;
    if handoff.write_response(&encoded).is_ok() {
        return Ok(());
    }
    handoff.write_response(&encoded_output_too_large_response())
}

#[cfg(feature = "adversarial-fixtures")]
fn write_test_marker(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(path)?;
    file.write_all(contents)?;
    file.flush()
}

#[cfg(feature = "adversarial-fixtures")]
#[doc(hidden)]
pub fn run_adversarial_fixture() -> i32 {
    use boa_engine::{Context, Source};
    use std::io::Write;

    let mut args = std::env::args_os().skip(1);
    let Some(mode) = args.next().and_then(|value| value.into_string().ok()) else {
        return 64;
    };
    if mode == "hidden-child" {
        loop {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    let handoff = match ChildHandoff::open_stdin() {
        Ok(value) => value,
        Err(_) => return 74,
    };
    match mode.as_str() {
        "boa-microtask-self-renew" => {
            let mut context = Context::default();
            if context
                .eval(Source::from_bytes(
                    "Promise.resolve().then(function f(){Promise.resolve().then(f)})",
                ))
                .is_err()
            {
                return 70;
            }
            context.run_jobs();
            0
        }
        "blocking-native" => loop {
            std::thread::sleep(Duration::from_secs(30));
        },
        "cpu-spin" => loop {
            std::hint::spin_loop();
        },
        "stderr-flood" => loop {
            if std::io::stderr().write_all(&[b'e'; 4096]).is_err() {
                return 74;
            }
        },
        "partial-response-hang" => {
            if handoff
                .corrupt_response(FixtureCorruption::MissingCompletion)
                .is_err()
            {
                return 74;
            }
            loop {
                std::thread::sleep(Duration::from_secs(30));
            }
        }
        "gated-marker" => {
            let (Some(readiness), Some(marker_due), Some(marker)) =
                (args.next(), args.next(), args.next())
            else {
                return 64;
            };
            let readiness_contents = format!("gated-marker-ready:{}", std::process::id());
            if write_test_marker(Path::new(&readiness), readiness_contents.as_bytes()).is_err() {
                return 74;
            }
            while !Path::new(&marker_due).exists() {
                std::thread::sleep(Duration::from_millis(2));
            }
            if write_test_marker(Path::new(&marker), b"watchdog missed deadline").is_err() {
                return 74;
            }
            loop {
                std::thread::sleep(Duration::from_secs(30));
            }
        }
        "exit-no-response" => 0,
        "missing-completion" => fixture_corruption(&handoff, FixtureCorruption::MissingCompletion),
        "bad-checksum" => fixture_corruption(&handoff, FixtureCorruption::BadChecksum),
        "oversized-length" => fixture_corruption(&handoff, FixtureCorruption::OversizedLength),
        "invalid-layout" => fixture_corruption(&handoff, FixtureCorruption::InvalidLayout),
        "grown-file" => fixture_corruption(&handoff, FixtureCorruption::GrownFile),
        "malformed-json" => fixture_corruption(&handoff, FixtureCorruption::MalformedJson),
        "wrong-version" => fixture_corruption(&handoff, FixtureCorruption::WrongProtocolVersion),
        "allocate-native" => {
            let mut chunks = Vec::new();
            loop {
                chunks.push(vec![0_u8; 8 * 1024 * 1024]);
            }
        }
        "grandchild-natural-exit" => {
            let Some(pid_file) = args.next() else {
                return 64;
            };
            let child = match spawn_hidden_fixture_child() {
                Ok(child) => child,
                Err(code) => return code,
            };
            let identities = format!("{}\n{}", std::process::id(), child.id());
            if std::fs::write(pid_file, identities).is_err() {
                return 74;
            }
            fixture_success(&handoff)
        }
        "grandchild" => {
            let Some(pid_file) = args.next() else {
                return 64;
            };
            let child = match spawn_hidden_fixture_child() {
                Ok(child) => child,
                Err(code) => return code,
            };
            if std::fs::write(pid_file, child.id().to_string()).is_err() {
                return 74;
            }
            loop {
                std::thread::sleep(Duration::from_secs(30));
            }
        }
        _ => 64,
    }
}

#[cfg(feature = "adversarial-fixtures")]
#[allow(clippy::zombie_processes)]
fn spawn_hidden_fixture_child() -> Result<std::process::Child, i32> {
    Command::new(std::env::current_exe().expect("fixture executable"))
        .arg("hidden-child")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| 71)
}

#[cfg(feature = "adversarial-fixtures")]
fn fixture_success(handoff: &ChildHandoff) -> i32 {
    let response = match helper_once(handoff) {
        Ok(response) => response,
        Err(message) => WireResponse {
            protocol_version: PROTOCOL_VERSION,
            body: ResponseBody::Error {
                kind: ResponseErrorKind::Script,
                message: bound_text(&message, 512),
            },
        },
    };
    write_helper_response(handoff, &response).map_or(74, |()| 0)
}

#[cfg(feature = "adversarial-fixtures")]
fn fixture_corruption(handoff: &ChildHandoff, corruption: FixtureCorruption) -> i32 {
    handoff.corrupt_response(corruption).map_or(74, |()| 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_response_rejects_unknown_fields() {
        let body =
            br#"{"protocol_version":2,"extra":true,"body":{"status":"ok","value":1,"logs":[]}}"#;
        assert!(serde_json::from_slice::<WireResponse>(body).is_err());
    }
}
