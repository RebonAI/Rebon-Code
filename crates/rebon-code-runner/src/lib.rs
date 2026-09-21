//! PTC / Code Mode one-shot Node executor.
//!
//! This crate is deliberately V8-free. Code Mode never needed rebon's embedded
//! JS runtime — it needs a *separate* process it is allowed to kill — so it
//! lives beside [`rebon_boa_runner`] (which owns the containment unit) rather
//! than inside the crate that hosts V8, and a build without the embedded
//! runtime still has Code Mode.
//!
//! Boa is deliberately not used here: the hardened Boa helper is synchronous and rejects genuine
//! Promise results, while Code Mode's ABI is an async-function body. Each run therefore starts a
//! fresh, restricted Node process that is independent of the long-lived plugin host. The child is
//! placed in an OS containment unit before its execute frame is admitted, and the supervisor owns
//! hard timeout/cancellation, descendant termination, and direct-child reaping.
//!
//! Unix containment adds a hard `RLIMIT_NOFILE=32`. Windows Job Objects have no numeric quota for
//! general kernel handles, so the guest is instead denied handle-creating filesystem, process,
//! module, and import capabilities; Job memory, CPU, and process limits plus the wall timeout bound
//! the impact and lifetime of the one-shot process.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use rebon_boa_runner::{
    observe_child_exit, reap_child_bounded, ChildExitObservation, ProcessContainment,
};
use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u16 = 1;
const NODE_ENV: &str = "REBON_CODE_MODE_NODE";
/// The spellings of the flag that turns Node's permission model on, newest
/// first.
///
/// Node renamed this when the model went stable and dropped the old spelling,
/// so no single string works across the runtimes Code Mode is handed: 24
/// rejects `--experimental-permission` outright, and a runtime old enough to
/// need it has never heard of `--permission`. Asking the runtime which one it
/// accepts costs one short-lived process, cannot drift when Node renames the
/// next flag, and — unlike a version table — is answered by the runtime that
/// will actually execute the program.
const PERMISSION_FLAGS: [&str; 2] = ["--permission", "--experimental-permission"];
const NODE_HELPER: &str = include_str!("../js/code_mode_runner.js");
const MAX_CODE_BYTES: usize = 64 * 1024;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// How large one tool result may be on its way *down* to the program.
///
/// Bigger than [`MAX_FRAME_BYTES`], and deliberately a separate number: the two
/// caps guard opposite directions. A frame the child sends up has to fit inside
/// [`MAX_TOTAL_STDOUT_BYTES`] along with everything else it will ever print, so
/// raising that one would only move the wall. Nothing downward is pooled — the
/// parent writes a tool result and the child reads it — so this one can be
/// generous, and reading a file is a large part of what Code Mode is for.
///
/// A result past even this is truncated rather than refused; see
/// [`tool_result_payload`].
const MAX_TOOL_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_LOG_BYTES: usize = 256 * 1024;
const MAX_LOG_LINES: usize = 1024;
const MAX_TOOL_CALLS: usize = 2048;
const STACK_BYTES: u64 = 4 * 1024 * 1024;
// No `RLIMIT_NPROC` for the Code Mode child.
//
// It looked like a cheap extra boundary and it is not one. The limit is counted
// per *user*, over every process and thread they already have, so it never
// bounded this child; and Node needs more of that budget at startup than the 32
// it was given. Measured on Fedora 43 with Node 22.22.2, a user with six live
// threads: at 32 the runtime aborts before it evaluates anything —
//
//     node::WorkerThreadsTaskRunner::DelayedTaskScheduler::Start()
//     Assertion failed: (0) == (uv_thread_create(t.get(), start_thread, this))
//
// which is SIGABRT, and is why Code Mode did not work on Linux at all. 64 boots.
// But any constant is a bet on what else the user is running, and losing that
// bet costs the whole feature rather than one program, so there is no constant
// worth choosing. This is the shape macOS already refuses `RLIMIT_AS` and
// `RLIMIT_STACK` for: a limit that measures something outside this process
// cannot say anything about it.
//
// What denies descendants is the permission model, checked rather than assumed:
// `--permission` answers `ERR_ACCESS_DENIED` to `child_process`. What bounds the
// blast radius is unchanged — address space, CPU, file descriptors, core size and
// stack are all still set, the child is its own process group, and the
// supervisor kills that group on the deadline.
const ACTIVE_PROCESS_LIMIT: u32 = 0;
static ACTIVE_CODE_EXECUTIONS: AtomicUsize = AtomicUsize::new(0);
const NODE_OLD_SPACE_MIB: u64 = 192;
#[cfg(windows)]
const MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
// V8 reserves a multi-GiB pointer-compression cage on Unix. RLIMIT_AS must admit that reservation;
// the 192-MiB V8 heap flag remains the tighter ordinary allocation bound.
#[cfg(unix)]
const MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
#[cfg(not(any(unix, windows)))]
const MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// Number of one-shot execution units that have been admitted and not yet reaped.
#[doc(hidden)]
pub fn active_code_executions() -> usize {
    ACTIVE_CODE_EXECUTIONS.load(Ordering::Acquire)
}

struct ActiveExecution;

impl ActiveExecution {
    fn acquire() -> Self {
        ACTIVE_CODE_EXECUTIONS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for ActiveExecution {
    fn drop(&mut self) {
        ACTIVE_CODE_EXECUTIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The embedder's tool-dispatch seam. Every `tools.name(args)` child frame lands here and then
/// re-enters the engine's normal validation and permission pipeline.
pub trait CodeToolDispatcher: Send + Sync {
    fn dispatch(
        &self,
        name: String,
        input: serde_json::Value,
    ) -> BoxFuture<'static, Result<serde_json::Value, String>>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodeOutcome {
    pub result: Option<serde_json::Value>,
    pub logs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CodeProgramError {
    pub message: String,
    pub logs: Vec<String>,
}

/// An absolute Node executable selected independently of plugin runtime
/// discovery, together with the proof that it can be hardened.
///
/// The sibling runtime a packaged install is expected to carry.
fn sibling_runtime_name() -> &'static str {
    if cfg!(windows) {
        "rebon-code-node.exe"
    } else {
        "rebon-code-node"
    }
}

/// Where [`CodeNodeRuntime::discover`] would look, without running anything.
///
/// The same two deterministic rungs, stopping short of the permission-flag
/// probe — which spawns a process, and so cannot sit on a path that decides
/// whether to *advertise* the tool. A caller that wants to know "is there a
/// runtime at all" pays two `stat`s for the answer; a caller that wants to
/// use one still goes through `discover`, which vets it.
///
/// Finding a path here does not promise the runtime can be hardened. It only
/// rules out the case where there is nothing to harden.
pub fn locate_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(NODE_ENV) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let current = std::env::current_exe().ok()?;
    let candidate = current.parent()?.join(sibling_runtime_name());
    candidate.is_file().then_some(candidate)
}

/// Constructing one runs the guest's own flag set against the runtime and keeps
/// the permission-flag spelling it accepted, so the check happens once per
/// runtime rather than once per program — and so a runtime that cannot be
/// hardened is refused where it is chosen, not where a model's program is
/// already waiting to run.
#[derive(Debug, Clone)]
pub struct CodeNodeRuntime {
    executable: PathBuf,
    working_directory: PathBuf,
    permission_flag: &'static str,
}

impl CodeNodeRuntime {
    pub fn new(executable: impl Into<PathBuf>) -> Result<Self, CodeProgramError> {
        let executable = executable.into();
        if !executable.is_absolute() {
            return Err(startup_error(
                "Code Mode Node executable must be an absolute path",
            ));
        }
        if !executable.is_file() {
            return Err(startup_error(format!(
                "Code Mode Node executable is missing: {}",
                executable.display()
            )));
        }
        let working_directory = executable
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| startup_error("Code Mode Node executable has no parent directory"))?;
        let permission_flag = probe_permission_flag(&executable, &working_directory)?;
        Ok(Self {
            executable,
            working_directory,
            permission_flag,
        })
    }

    /// Production discovery is intentionally deterministic: an absolute launcher-provided path,
    /// then a specifically named sibling runtime. It never searches `PATH` or plugin installs.
    pub fn discover() -> Result<Self, CodeProgramError> {
        match locate_executable() {
            Some(path) => Self::new(path),
            None => Err(startup_error(format!(
                "Code Mode Node runtime is unavailable; the packaged launcher must set {NODE_ENV} to its absolute process.execPath (or bundle {} beside Rebon)",
                sibling_runtime_name()
            ))),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The permission-flag spelling this runtime accepted. Diagnostic only —
    /// the spawn reads the field directly.
    pub fn permission_flag(&self) -> &'static str {
        self.permission_flag
    }
}

/// A Node invocation with nothing inherited from rebon's own environment.
///
/// `env_clear` is part of the sandbox: the guest must not see API keys, proxy
/// settings, or anything else that reached rebon's process. Windows needs three
/// variables back or the loader cannot find its own system DLLs, which is a
/// property of the OS rather than a hole in the sandbox.
fn base_command(executable: &Path, working_directory: &Path) -> Command {
    let mut command = Command::new(executable);
    command.current_dir(working_directory).env_clear();
    #[cfg(windows)]
    for name in ["SystemRoot", "SystemDrive", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// The flags that make the guest a guest.
///
/// Every one of these is load-bearing, so they are written once and used by both
/// the probe and the spawn: a runtime is accepted only if it accepts exactly the
/// set it will later be run with.
fn harden(command: &mut Command, permission_flag: &str, stack_bytes: u64) {
    command
        .arg("--no-warnings")
        .arg(permission_flag)
        .arg("--experimental-vm-modules")
        .arg("--no-addons")
        .arg("--disable-proto=throw")
        .arg(format!("--max-old-space-size={NODE_OLD_SPACE_MIB}"))
        .arg(format!("--stack_size={}", v8_stack_kib(stack_bytes)));
}

/// What to tell V8 its stack is, given what the OS will actually allow.
///
/// Deliberately below the `RLIMIT_STACK` the containment installs, because V8
/// has to notice first. Told it owns the whole rlimit, it keeps recursing until
/// the kernel faults, and a program that recursed too deep dies as SIGSEGV with
/// nothing to report instead of raising `RangeError: Maximum call stack size
/// exceeded` — which the program can catch and the run can explain. Seen on
/// Linux, where the two numbers were equal; Windows never had an rlimit to
/// collide with, so it always had the headroom this makes explicit.
///
/// Three quarters, so the margin scales with the limit rather than being a
/// constant that a smaller stack would swallow.
fn v8_stack_kib(stack_bytes: u64) -> u64 {
    (stack_bytes / 1024) * 3 / 4
}

/// Which permission-flag spelling this runtime accepts, or a refusal.
///
/// Fail-closed by construction: the loop returns a flag only when the runtime
/// started with the whole hardened set, and no rung drops the permission flag to
/// make an unsupported runtime work. Code Mode executes code the model wrote;
/// running it with the sandbox silently absent is worse than not running it.
fn probe_permission_flag(
    executable: &Path,
    working_directory: &Path,
) -> Result<&'static str, CodeProgramError> {
    let mut rejected = Vec::new();
    for flag in PERMISSION_FLAGS {
        let mut command = base_command(executable, working_directory);
        harden(&mut command, flag, STACK_BYTES);
        // `0` is the smallest program that proves the runtime got as far as
        // evaluating one. It reads, writes, and resolves nothing, so a failure
        // here is about the flags rather than about what the program did.
        command
            .arg("-e")
            .arg("0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.status() {
            Ok(status) if status.success() => return Ok(flag),
            Ok(status) => rejected.push(format!("{flag} ({status})")),
            Err(error) => {
                return Err(startup_error(format!(
                    "cannot start the Code Mode Node runtime at {}: {error}",
                    executable.display()
                )))
            }
        }
    }
    Err(startup_error(format!(
        "the Node runtime at {} accepts none of Code Mode's hardening flags (tried {}); \
         Code Mode runs model-authored code and will not run it without Node's permission \
         model, so this runtime is refused rather than used unsandboxed",
        executable.display(),
        rejected.join(", ")
    )))
}

fn startup_error(message: impl Into<String>) -> CodeProgramError {
    CodeProgramError {
        message: message.into(),
        logs: Vec::new(),
    }
}

#[derive(Debug, Clone)]
struct ExecutionLimits {
    wall_time: Duration,
    memory_bytes: u64,
    cpu_seconds: u64,
    stack_bytes: u64,
    max_output_bytes: usize,
    max_log_bytes: usize,
    max_log_lines: usize,
}

impl ExecutionLimits {
    fn for_budget(wall_time: Duration) -> Result<Self, CodeProgramError> {
        if wall_time.is_zero() || Instant::now().checked_add(wall_time).is_none() {
            return Err(startup_error("Code Mode budget must be finite and nonzero"));
        }
        let cpu_seconds = wall_time
            .as_secs()
            .checked_add(u64::from(wall_time.subsec_nanos() != 0))
            .and_then(|seconds| seconds.checked_add(1))
            .ok_or_else(|| startup_error("Code Mode budget cannot derive a finite CPU limit"))?;
        Ok(Self {
            wall_time,
            memory_bytes: MEMORY_BYTES,
            cpu_seconds,
            stack_bytes: STACK_BYTES,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_log_bytes: MAX_LOG_BYTES,
            max_log_lines: MAX_LOG_LINES,
        })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ExecuteFrame<'a> {
    v: u16,
    #[serde(rename = "type")]
    kind: &'static str,
    code: &'a str,
    limits: ExecuteFrameLimits,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecuteFrameLimits {
    max_output_bytes: usize,
    max_log_bytes: usize,
    max_log_lines: usize,
    /// The cap on a frame the child sends *up*, so the helper does not keep its
    /// own copy of a number Rust already owns. Two copies of a limit is two
    /// numbers that can disagree, and the disagreement would show up as a frame
    /// one side wrote and the other refused.
    max_frame_bytes: usize,
    /// The cap on a frame the child reads *down*. Larger, because a tool result
    /// is the one thing that legitimately gets big; if the child kept using the
    /// upward cap here it would drop exactly the large results Code Mode exists
    /// to hand over.
    max_incoming_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum ChildFrame {
    #[serde(rename = "tool_call")]
    ToolCall {
        v: u16,
        id: u64,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "terminal")]
    Terminal {
        v: u16,
        ok: bool,
        #[serde(default, rename = "resultUndefined")]
        result_undefined: bool,
        #[serde(default)]
        result: serde_json::Value,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        message: Option<String>,
        logs: Vec<String>,
    },
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ToolResultFrame<'a> {
    v: u16,
    #[serde(rename = "type")]
    kind: &'static str,
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

enum ReaderEvent {
    Frame(Vec<u8>),
    Error(String),
}

enum FinishKind {
    Natural(ExitStatus),
    Timeout,
    Cancelled,
    Failed(String),
}

struct SupervisorResult {
    child_pid: u32,
    finish: FinishKind,
}

struct SupervisorStart {
    child: Child,
    containment: ProcessContainment,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    io_stop: Arc<AtomicBool>,
    active_execution: ActiveExecution,
}

struct ExecutionProcess {
    child_pid: u32,
    events: tokio::sync::mpsc::Receiver<ReaderEvent>,
    responses: Option<std_mpsc::Sender<Vec<u8>>>,
    done: tokio::sync::oneshot::Receiver<SupervisorResult>,
    cancelled: Arc<AtomicBool>,
    supervisor: Option<JoinHandle<()>>,
}

impl ExecutionProcess {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn finish(mut self) {
        self.responses.take();
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ExecutionProcess {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // Closing the frame channel is what makes the join below finite.
        //
        // The reader thread checks the stop flag between reads, so a reader
        // parked in `blocking_send` never sees it — and that is exactly where a
        // cancelled run leaves it, because the frames its child already queued
        // have no one left to drain them. The supervisor joins the reader before
        // it reports, so without this the join here waits for a thread that is
        // waiting for a receiver this very value is about to drop.
        self.events.close();
        self.responses.take();
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.join();
        }
    }
}

pub async fn run_code_program(
    code: String,
    dispatcher: Arc<dyn CodeToolDispatcher>,
    budget: Duration,
) -> Result<CodeOutcome, CodeProgramError> {
    let runtime = CodeNodeRuntime::discover()?;
    run_code_program_with_runtime(code, dispatcher, budget, runtime).await
}

pub async fn run_code_program_with_runtime(
    code: String,
    dispatcher: Arc<dyn CodeToolDispatcher>,
    budget: Duration,
    runtime: CodeNodeRuntime,
) -> Result<CodeOutcome, CodeProgramError> {
    if code.len() > MAX_CODE_BYTES {
        return Err(startup_error(format!(
            "program exceeds the {MAX_CODE_BYTES}-byte Code Mode source limit"
        )));
    }
    let limits = ExecutionLimits::for_budget(budget)?;
    // A start has a blocking half and a cheap one, and only the blocking half
    // leaves the caller's task: process creation plus the containment handshake
    // measure about 20ms of synchronous kernel work on Windows, which is not
    // something a runtime worker should sit through once per program.
    //
    // The split is at the frame write rather than at the end, so the value this
    // task owns is still the execution unit itself. Dropping this future
    // therefore tears the unit down here and now — the guarantee the cancel path
    // depends on — while a child whose caller left during the blocking half is
    // killed and reaped by `StartedChild`'s own `Drop`.
    let started = {
        let limits = limits.clone();
        tokio::task::spawn_blocking(move || start_child(&runtime, &code, &limits))
            .await
            .map_err(|error| {
                startup_error(format!("Code Mode start task did not complete: {error}"))
            })??
    };
    let mut process = attach_execution(started)?;
    let mut terminal: Option<Result<CodeOutcome, CodeProgramError>> = None;
    let mut protocol_failure: Option<String> = None;
    let mut dispatches = tokio::task::JoinSet::new();
    // What had to be shortened on the way to the program, so the answer can
    // say so. A program that ran to completion on a cut-down file is not the
    // same as one that ran on the whole thing, and the model is the one who
    // has to know the difference.
    let truncations: std::sync::Arc<std::sync::Mutex<Vec<Truncation>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut tool_call_ids = HashSet::new();
    // Cleared once the child's output ends. The run is not over at that point —
    // the supervisor still owes a result — and a channel that has closed reports
    // "no frame" the instant it is asked, so a branch that kept polling it would
    // spin at full tilt until the supervisor got there.
    let mut frames_open = true;

    let supervisor = loop {
        tokio::select! {
            biased;
            done = &mut process.done => {
                break done.unwrap_or(SupervisorResult {
                    child_pid: process.child_pid,
                    finish: FinishKind::Failed("Code Mode supervisor ended without a result".into()),
                });
            }
            event = process.events.recv(), if frames_open => {
                let Some(event) = event else {
                    frames_open = false;
                    continue;
                };
                match event {
                    ReaderEvent::Error(message) => {
                        protocol_failure.get_or_insert(message);
                        process.cancel();
                    }
                    ReaderEvent::Frame(bytes) => {
                        let parsed: ChildFrame = match serde_json::from_slice(&bytes) {
                            Ok(frame) => frame,
                            Err(_) => {
                                protocol_failure.get_or_insert_with(|| "invalid or unknown Code Mode child frame".into());
                                process.cancel();
                                continue;
                            }
                        };
                        match parsed {
                            ChildFrame::ToolCall { v, id, name, input } => {
                                if v != PROTOCOL_VERSION
                                    || terminal.is_some()
                                    || name.is_empty()
                                    || name.len() > 256
                                    || !tool_call_ids.insert(id)
                                    || tool_call_ids.len() > MAX_TOOL_CALLS
                                {
                                    protocol_failure.get_or_insert_with(|| "invalid Code Mode tool-call frame".into());
                                    process.cancel();
                                    continue;
                                }
                                let dispatcher = dispatcher.clone();
                                let responses = process.responses.as_ref().expect("response channel").clone();
                                let truncations = truncations.clone();
                                dispatches.spawn(async move {
                                    let outcome = dispatcher.dispatch(name.clone(), input).await;
                                    let shortened = match &outcome {
                                        Ok(value) => {
                                            let (payload, cut) = tool_result_payload(
                                                &name,
                                                value,
                                                MAX_TOOL_RESULT_BYTES,
                                            );
                                            if let Some(cut) = cut {
                                                // One line on purpose: a
                                                // line-based check only
                                                // recognises the `into_inner`
                                                // recovery on a lock whose
                                                // whole call sits on one line.
                                                let mut cuts = truncations.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                                cuts.push(cut);
                                            }
                                            Some(payload)
                                        }
                                        Err(_) => None,
                                    };
                                    let frame = match (&outcome, &shortened) {
                                        (Ok(_), Some(value)) => ToolResultFrame {
                                            v: PROTOCOL_VERSION,
                                            kind: "tool_result",
                                            id,
                                            ok: true,
                                            value: Some(value),
                                            error: None,
                                        },
                                        _ => ToolResultFrame {
                                            v: PROTOCOL_VERSION,
                                            kind: "tool_result",
                                            id,
                                            ok: false,
                                            value: None,
                                            error: Some(outcome.as_ref().err().map_or("tool dispatch failed", |error| error.as_str())),
                                        },
                                    };
                                    // The payload is already inside the limit, so this
                                    // encodes. The fallback stays for a value that is
                                    // small but somehow unserialisable, which is a
                                    // different fault and still must not hang the
                                    // program waiting for a frame that never comes.
                                    let encoded = encode_parent_frame_within(&frame, MAX_TOOL_RESULT_BYTES)
                                        .unwrap_or_else(|message| {
                                            encode_parent_frame(&ToolResultFrame {
                                                v: PROTOCOL_VERSION,
                                                kind: "tool_result",
                                                id,
                                                ok: false,
                                                value: None,
                                                error: Some(&message),
                                            }).expect("bounded fallback tool-result frame")
                                        });
                                    let _ = responses.send(encoded);
                                });
                            }
                            ChildFrame::Terminal {
                                v,
                                ok,
                                result_undefined,
                                result,
                                kind,
                                message,
                                logs,
                            } => {
                                if v != PROTOCOL_VERSION
                                    || terminal.is_some()
                                    || !valid_logs(&logs, &limits)
                                    || !valid_terminal_fields(
                                        ok,
                                        result_undefined,
                                        &result,
                                        kind.as_deref(),
                                        message.as_deref(),
                                    )
                                {
                                    protocol_failure.get_or_insert_with(|| "invalid or duplicate Code Mode terminal frame".into());
                                    process.cancel();
                                    continue;
                                }
                                terminal = Some(if ok {
                                    if kind.is_some() || message.is_some() {
                                        Err(CodeProgramError { message: "invalid successful terminal frame".into(), logs })
                                    } else {
                                        Ok(CodeOutcome {
                                            result: (!result_undefined).then_some(result),
                                            logs,
                                        })
                                    }
                                } else {
                                    Err(CodeProgramError {
                                        message: format!(
                                            "{}: {}",
                                            kind.as_deref().unwrap_or("runtime"),
                                            message.as_deref().unwrap_or("JavaScript execution failed")
                                        ),
                                        logs,
                                    })
                                });
                            }
                        }
                    }
                }
            }
        }
    };

    dispatches.abort_all();
    while dispatches.join_next().await.is_some() {}
    process.finish();

    if let Some(message) = protocol_failure {
        return Err(CodeProgramError {
            message: format!("Code Mode protocol failure: {message}"),
            logs: Vec::new(),
        });
    }
    match supervisor.finish {
        FinishKind::Timeout => Err(CodeProgramError {
            message: format!(
                "program exceeded its {}ms budget and execution unit {} was killed and reaped",
                budget.as_millis(), supervisor.child_pid
            ),
            logs: terminal_logs(&terminal),
        }),
        FinishKind::Cancelled => Err(CodeProgramError {
            message: format!(
                "program execution was cancelled; execution unit {} was killed and reaped",
                supervisor.child_pid
            ),
            logs: terminal_logs(&terminal),
        }),
        FinishKind::Failed(message) => Err(CodeProgramError {
            message,
            logs: terminal_logs(&terminal),
        }),
        FinishKind::Natural(status) => match terminal {
            Some(Ok(outcome)) if status.success() => Ok(note_truncations(outcome, &truncations)),
            Some(Ok(_)) => Err(CodeProgramError {
                message: format!("Code Mode child exited unsuccessfully: {status}"),
                logs: Vec::new(),
            }),
            Some(Err(error)) => Err(error),
            None => Err(CodeProgramError {
                message: format!(
                    "Code Mode child exited without one terminal result (status {status}); a resource limit may have terminated it"
                ),
                logs: Vec::new(),
            }),
        },
    }
}

fn terminal_logs(terminal: &Option<Result<CodeOutcome, CodeProgramError>>) -> Vec<String> {
    match terminal {
        Some(Ok(outcome)) => outcome.logs.clone(),
        Some(Err(error)) => error.logs.clone(),
        None => Vec::new(),
    }
}

fn valid_terminal_fields(
    ok: bool,
    result_undefined: bool,
    result: &serde_json::Value,
    kind: Option<&str>,
    message: Option<&str>,
) -> bool {
    if ok {
        kind.is_none() && message.is_none() && (!result_undefined || result.is_null())
    } else {
        !result_undefined
            && result.is_null()
            && matches!(
                kind,
                Some("syntax" | "runtime" | "protocol" | "output_limit")
            )
            && message.is_some_and(|message| !message.is_empty())
    }
}

fn valid_logs(logs: &[String], limits: &ExecutionLimits) -> bool {
    logs.len() <= limits.max_log_lines
        && logs
            .iter()
            .try_fold(0_usize, |total, line| total.checked_add(line.len()))
            .is_some_and(|total| total <= limits.max_log_bytes)
}

fn encode_parent_frame(frame: &impl Serialize) -> Result<Vec<u8>, String> {
    encode_parent_frame_within(frame, MAX_FRAME_BYTES)
}

fn encode_parent_frame_within(frame: &impl Serialize, limit: usize) -> Result<Vec<u8>, String> {
    let mut encoded =
        serde_json::to_vec(frame).map_err(|_| "tool result is not JSON".to_string())?;
    if encoded.len() + 1 > limit {
        return Err(format!(
            "the frame is {} bytes, over the {limit}-byte limit",
            encoded.len() + 1
        ));
    }
    encoded.push(b'\n');
    Ok(encoded)
}

/// How much of a too-large result was kept, for the note the model reads.
struct Truncation {
    tool: String,
    original_bytes: usize,
    kept_bytes: usize,
}

/// The value a tool result carries down to the program, shortened if it has to
/// be.
///
/// A result over the limit used to be refused, and the refusal reached the
/// program as a rejected promise — so a program that read one large file lost
/// every result it had already collected and every line it had left to run, and
/// the model was told only that something exceeded a limit. Which tool, and how
/// far over, it was not told.
///
/// Truncating instead keeps the program running, which is the point of Code
/// Mode: it is there to do work the model would otherwise do one tool call at a
/// time. The marker is an object rather than a bare shortened string because a
/// program has to be able to *notice* — `truncated` is the field to branch on,
/// and a silently shortened string would be indistinguishable from a small one.
///
/// The kept text is cut on a UTF-8 boundary. A non-string value is rendered as
/// JSON text first and cut the same way, since there is no general way to cut a
/// structure and leave it valid.
fn tool_result_payload(
    tool: &str,
    value: &serde_json::Value,
    limit: usize,
) -> (serde_json::Value, Option<Truncation>) {
    let original = serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if original + FRAME_OVERHEAD_BYTES <= limit {
        return (value.clone(), None);
    }
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    // Room for the marker's own fields, so the wrapper cannot itself overflow.
    let room = limit.saturating_sub(FRAME_OVERHEAD_BYTES + MARKER_OVERHEAD_BYTES);
    let mut end = room.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &text[..end];
    (
        serde_json::json!({
            "truncated": true,
            "tool": tool,
            "originalBytes": original,
            "keptBytes": kept.len(),
            "limitBytes": limit,
            "value": kept,
        }),
        Some(Truncation {
            tool: tool.to_string(),
            original_bytes: original,
            kept_bytes: kept.len(),
        }),
    )
}

/// Appends one line per shortened result to what the program produced.
///
/// In the logs, which is where the run's own remarks already go and which the
/// renderer puts in front of the result: a reader who sees only the answer
/// would otherwise act on a partial one believing it was whole.
fn note_truncations(
    mut outcome: CodeOutcome,
    truncations: &std::sync::Mutex<Vec<Truncation>>,
) -> CodeOutcome {
    let cuts = truncations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for cut in cuts.iter() {
        outcome.logs.push(format!(
            "[truncated] {} returned {} bytes; the program was given the first {}",
            cut.tool, cut.original_bytes, cut.kept_bytes
        ));
    }
    outcome
}

/// The frame's own JSON around the value, plus the newline.
const FRAME_OVERHEAD_BYTES: usize = 128;
/// The marker's keys and numbers around the kept text.
const MARKER_OVERHEAD_BYTES: usize = 256;

/// A contained child that has been handed its execute frame and nothing else.
///
/// The blocking half of a start, kept as its own value so the cheap half can
/// happen somewhere else. A child nobody goes on to attach to is killed and
/// reaped by this value's `Drop`, which is what makes the handoff safe to
/// interrupt: the caller can walk away between the halves without leaving a Node
/// process behind.
struct StartedChild {
    parts: Option<StartedParts>,
    child_pid: u32,
    deadline: Instant,
}

struct StartedParts {
    child: Child,
    containment: ProcessContainment,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl StartedChild {
    /// Hands the child over. The `Drop` below is a no-op afterwards.
    fn claim(mut self) -> (StartedParts, u32, Instant) {
        let parts = self
            .parts
            .take()
            .expect("a started child is claimed exactly once");
        (parts, self.child_pid, self.deadline)
    }
}

impl Drop for StartedChild {
    fn drop(&mut self) {
        if let Some(mut parts) = self.parts.take() {
            cleanup_failed_start(&mut parts.child, &mut parts.containment);
        }
    }
}

/// Creates the contained child and admits its execute frame.
///
/// Every blocking call in a start is here: process creation, the containment
/// handshake, and the frame write. Containment is established before the frame
/// is admitted, so the child is never asked to run anything while it is still
/// unbounded.
fn start_child(
    runtime: &CodeNodeRuntime,
    code: &str,
    limits: &ExecutionLimits,
) -> Result<StartedChild, CodeProgramError> {
    let execute = ExecuteFrame {
        v: PROTOCOL_VERSION,
        kind: "execute",
        code,
        limits: ExecuteFrameLimits {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_incoming_bytes: MAX_TOOL_RESULT_BYTES,
            max_output_bytes: limits.max_output_bytes,
            max_log_bytes: limits.max_log_bytes,
            max_log_lines: limits.max_log_lines,
        },
    };
    let request = encode_parent_frame(&execute).map_err(startup_error)?;
    if request.len() > MAX_FRAME_BYTES {
        return Err(startup_error(
            "Code Mode execute frame exceeds the hard limit",
        ));
    }

    let mut command = base_command(&runtime.executable, &runtime.working_directory);
    harden(&mut command, runtime.permission_flag, limits.stack_bytes);
    command
        .arg("-e")
        .arg(NODE_HELPER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    ProcessContainment::configure_command_with_stack(
        &mut command,
        limits.memory_bytes,
        limits.cpu_seconds,
        ACTIVE_PROCESS_LIMIT,
        Some(limits.stack_bytes),
    )
    .map_err(|error| startup_error(format!("configure Code Mode containment: {error}")))?;

    let started = Instant::now();
    let deadline = started
        .checked_add(limits.wall_time)
        .ok_or_else(|| startup_error("Code Mode deadline is not representable"))?;
    let mut child = command
        .spawn()
        .map_err(|error| startup_error(format!("start Code Mode Node runtime: {error}")))?;
    let child_pid = child.id();
    let mut containment = match ProcessContainment::establish(
        &mut child,
        limits.memory_bytes,
        limits.cpu_seconds,
        ACTIVE_PROCESS_LIMIT,
    ) {
        Ok(containment) => containment,
        Err(error) => {
            let _ = child.kill();
            let _ = reap_child_bounded(&mut child, Duration::from_secs(2));
            return Err(startup_error(format!(
                "establish Code Mode containment: {error}"
            )));
        }
    };
    let mut stdin = child.stdin.take().ok_or_else(|| {
        cleanup_failed_start(&mut child, &mut containment);
        startup_error("Code Mode child stdin was not piped")
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        cleanup_failed_start(&mut child, &mut containment);
        startup_error("Code Mode child stdout was not piped")
    })?;
    if let Err(error) = stdin.write_all(&request).and_then(|()| stdin.flush()) {
        cleanup_failed_start(&mut child, &mut containment);
        return Err(startup_error(format!(
            "admit Code Mode execute frame: {error}"
        )));
    }

    Ok(StartedChild {
        parts: Some(StartedParts {
            child,
            containment,
            stdin,
            stdout,
        }),
        child_pid,
        deadline,
    })
}

/// Wires a started child to its I/O threads and its supervisor.
///
/// Nothing here blocks, so it stays on the caller's own task: the value it
/// returns is what owns the execution unit, and dropping it is what stops one.
fn attach_execution(started: StartedChild) -> Result<ExecutionProcess, CodeProgramError> {
    let (
        StartedParts {
            mut child,
            mut containment,
            stdin,
            stdout,
        },
        child_pid,
        deadline,
    ) = started.claim();

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    let (response_tx, response_rx) = std_mpsc::channel();
    let io_stop = Arc::new(AtomicBool::new(false));
    let reader_stop = io_stop.clone();
    let reader = std::thread::Builder::new()
        .name("code-node-reader".into())
        .spawn(move || read_child_frames(stdout, event_tx, reader_stop))
        .map_err(|error| {
            cleanup_failed_start(&mut child, &mut containment);
            startup_error(format!("start Code Mode frame reader: {error}"))
        })?;
    let writer_stop = io_stop.clone();
    let writer = match std::thread::Builder::new()
        .name("code-node-writer".into())
        .spawn(move || write_child_frames(stdin, response_rx, writer_stop))
    {
        Ok(writer) => writer,
        Err(error) => {
            io_stop.store(true, Ordering::Release);
            cleanup_failed_start(&mut child, &mut containment);
            let _ = reader.join();
            return Err(startup_error(format!(
                "start Code Mode frame writer: {error}"
            )));
        }
    };

    let cancelled = Arc::new(AtomicBool::new(false));
    let supervisor_cancelled = cancelled.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let (start_tx, start_rx) = std_mpsc::sync_channel::<SupervisorStart>(0);
    let start = SupervisorStart {
        child,
        containment,
        reader,
        writer,
        io_stop,
        active_execution: ActiveExecution::acquire(),
    };
    let supervisor = match std::thread::Builder::new()
        .name("code-node-supervisor".into())
        .spawn(move || {
            let Ok(start) = start_rx.recv() else {
                return;
            };
            let SupervisorStart {
                mut child,
                mut containment,
                reader,
                writer,
                io_stop,
                active_execution,
            } = start;
            let provisional = loop {
                if Instant::now() >= deadline {
                    break FinishKind::Timeout;
                }
                if supervisor_cancelled.load(Ordering::Acquire) {
                    break FinishKind::Cancelled;
                }
                match observe_child_exit(&mut child) {
                    Ok(ChildExitObservation::Running) => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Ok(ChildExitObservation::Exited) => break FinishKind::Natural(success_status()),
                    Err(error) => {
                        break FinishKind::Failed(format!("observe Code Mode child: {error}"))
                    }
                }
            };
            let natural = matches!(provisional, FinishKind::Natural(_));
            let mut finish = provisional;
            if let Err(error) = containment.terminate_and_seal(natural) {
                finish = FinishKind::Failed(format!("seal Code Mode containment: {error}"));
            }
            let reaped = reap_child_bounded(&mut child, Duration::from_secs(2));
            finish = match (finish, reaped) {
                (FinishKind::Natural(_), Ok(Some(status))) => FinishKind::Natural(status),
                (other, Ok(Some(_))) => other,
                (_, Ok(None)) => {
                    FinishKind::Failed("Code Mode child could not be reaped within 2s".into())
                }
                (_, Err(error)) => FinishKind::Failed(format!("reap Code Mode child: {error}")),
            };
            io_stop.store(true, Ordering::Release);
            let _ = writer.join();
            let _ = reader.join();
            drop(active_execution);
            let _ = done_tx.send(SupervisorResult { child_pid, finish });
        }) {
        Ok(supervisor) => supervisor,
        Err(error) => {
            cleanup_supervisor_start(start);
            return Err(startup_error(format!(
                "start Code Mode supervisor: {error}"
            )));
        }
    };
    if let Err(std_mpsc::SendError(start)) = start_tx.send(start) {
        cleanup_supervisor_start(start);
        let _ = supervisor.join();
        return Err(startup_error(
            "Code Mode supervisor closed before accepting execution ownership",
        ));
    }

    Ok(ExecutionProcess {
        child_pid,
        events: event_rx,
        responses: Some(response_tx),
        done: done_rx,
        cancelled,
        supervisor: Some(supervisor),
    })
}

// Placeholder status used only until the unreaped child's real status is collected after sealing.
fn success_status() -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        unreachable!("Code Mode containment is supported only on Unix and Windows")
    }
}

fn cleanup_supervisor_start(mut start: SupervisorStart) {
    start.io_stop.store(true, Ordering::Release);
    cleanup_failed_start(&mut start.child, &mut start.containment);
    let _ = start.writer.join();
    let _ = start.reader.join();
}

fn cleanup_failed_start(child: &mut Child, containment: &mut ProcessContainment) {
    let _ = containment.terminate_and_seal(false);
    let _ = reap_child_bounded(child, Duration::from_secs(2));
}

fn write_child_frames(
    mut stdin: ChildStdin,
    receiver: std_mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(frame) => {
                if stdin
                    .write_all(&frame)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    return;
                }
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn read_child_frames(
    mut stdout: impl Read,
    sender: tokio::sync::mpsc::Sender<ReaderEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut frame = Vec::new();
    let mut total = 0_usize;
    let mut chunk = [0_u8; 8192];
    while !stop.load(Ordering::Acquire) {
        let read = match stdout.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) => {
                let _ = sender.blocking_send(ReaderEvent::Error(format!(
                    "read Code Mode child frame: {error}"
                )));
                return;
            }
        };
        total = match total.checked_add(read) {
            Some(total) if total <= MAX_TOTAL_STDOUT_BYTES => total,
            _ => {
                let _ = sender.blocking_send(ReaderEvent::Error(
                    "Code Mode child exceeded the aggregate stdout cap".into(),
                ));
                return;
            }
        };
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if frame.is_empty() {
                    let _ = sender.blocking_send(ReaderEvent::Error(
                        "Code Mode child emitted an empty frame".into(),
                    ));
                    return;
                }
                let complete = std::mem::take(&mut frame);
                if sender.blocking_send(ReaderEvent::Frame(complete)).is_err() {
                    return;
                }
            } else {
                frame.push(*byte);
                if frame.len() >= MAX_FRAME_BYTES {
                    let _ = sender.blocking_send(ReaderEvent::Error(
                        "Code Mode child frame exceeds the hard limit".into(),
                    ));
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Node these tests run a real execution unit on.
    fn test_runtime() -> CodeNodeRuntime {
        let output = Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .expect("Node is required for Code Mode tests");
        assert!(
            output.status.success(),
            "node could not report its own path"
        );
        let path = String::from_utf8(output.stdout).expect("a Node path is UTF-8");
        CodeNodeRuntime::new(PathBuf::from(path.trim())).expect("a hardenable Node runtime")
    }

    /// Walking away from a chatty program releases the thread that walks.
    ///
    /// A cancelled turn drops the run's future while its child may be mid-burst,
    /// and the frames already queued then have no reader on the other end. The
    /// drop has to break that standoff itself: the reader is parked handing over
    /// a frame, the supervisor will not report until the reader is joined, and
    /// the drop will not return until the supervisor does. The child is asked
    /// for far more frames than the channel holds so the reader is certainly
    /// parked in one, and the drop runs on a thread of its own so a failure is
    /// an assertion rather than a hung test run.
    #[test]
    fn dropping_a_run_mid_burst_does_not_wedge_the_dropping_thread() {
        let runtime = test_runtime();
        let limits = ExecutionLimits::for_budget(Duration::from_secs(60)).expect("limits");
        // Unawaited on purpose: each call still emits its frame, and the program
        // then stays alive so the child is never the thing that ends the run.
        let code = "for (let i = 0; i < 500; i += 1) { tools.noop({ i }); }\n\
                    await new Promise(() => {});";
        let started = start_child(&runtime, code, &limits).expect("a started child");
        let process = attach_execution(started).expect("execution unit");
        // Long enough for the child to boot and fill the channel past capacity.
        std::thread::sleep(Duration::from_millis(2000));

        let (finished_tx, finished_rx) = std_mpsc::channel();
        std::thread::Builder::new()
            .name("code-node-drop-probe".into())
            .spawn(move || {
                drop(process);
                let _ = finished_tx.send(());
            })
            .expect("drop probe thread");
        assert!(
            finished_rx.recv_timeout(Duration::from_secs(20)).is_ok(),
            "dropping a mid-burst execution unit never returned"
        );
    }

    #[test]
    fn runtime_requires_an_absolute_existing_executable() {
        assert!(CodeNodeRuntime::new("node").is_err());
        let missing = if cfg!(windows) {
            PathBuf::from(r"C:\definitely-missing\node.exe")
        } else {
            PathBuf::from("/definitely-missing/node")
        };
        assert!(CodeNodeRuntime::new(missing).is_err());
    }

    #[test]
    fn child_protocol_rejects_unknown_and_version_mismatched_frames() {
        let unknown = br#"{"v":1,"type":"terminal","ok":true,"logs":[],"extra":1}"#;
        assert!(serde_json::from_slice::<ChildFrame>(unknown).is_err());
        let mismatched = br#"{"v":999,"type":"terminal","ok":true,"logs":[]}"#;
        let ChildFrame::Terminal { v, .. } =
            serde_json::from_slice::<ChildFrame>(mismatched).unwrap()
        else {
            panic!("terminal frame expected")
        };
        assert_ne!(v, PROTOCOL_VERSION);
    }

    #[test]
    fn child_frame_reader_rejects_oversize_frames() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        read_child_frames(
            std::io::Cursor::new(vec![b'x'; MAX_FRAME_BYTES]),
            sender,
            Arc::new(AtomicBool::new(false)),
        );
        match receiver.blocking_recv().expect("reader event") {
            ReaderEvent::Error(message) => assert!(message.contains("frame exceeds"), "{message}"),
            ReaderEvent::Frame(_) => panic!("oversize input must not become a frame"),
        }
    }

    #[test]
    fn terminal_frame_fields_are_semantically_strict() {
        assert!(valid_terminal_fields(
            true,
            false,
            &serde_json::json!({"ok": true}),
            None,
            None,
        ));
        assert!(valid_terminal_fields(
            true,
            true,
            &serde_json::Value::Null,
            None,
            None,
        ));
        assert!(valid_terminal_fields(
            false,
            false,
            &serde_json::Value::Null,
            Some("runtime"),
            Some("failed"),
        ));
        assert!(!valid_terminal_fields(
            true,
            true,
            &serde_json::json!(1),
            None,
            None,
        ));
        assert!(!valid_terminal_fields(
            false,
            false,
            &serde_json::Value::Null,
            Some("unknown"),
            Some("failed"),
        ));
    }

    /// A result that used to fail the whole program now reaches it whole.
    ///
    /// 1.5 MiB is over the old frame limit and under the new tool-result one,
    /// which is the size that produced the original failure: a program read one
    /// ordinary file and lost every result it had already gathered.
    #[test]
    fn a_result_over_the_old_limit_is_passed_through_untouched() {
        let value = serde_json::json!("x".repeat(3 * 512 * 1024));
        let (payload, cut) = tool_result_payload("Read", &value, MAX_TOOL_RESULT_BYTES);
        assert!(cut.is_none(), "nothing needed cutting");
        assert_eq!(
            payload, value,
            "the program gets exactly what the tool returned"
        );
    }

    /// Past even the new limit it is shortened, not refused, and says so.
    #[test]
    fn a_result_over_the_new_limit_is_truncated_and_marked() {
        let value = serde_json::json!("y".repeat(MAX_TOOL_RESULT_BYTES + 4096));
        let (payload, cut) = tool_result_payload("Read", &value, MAX_TOOL_RESULT_BYTES);

        let cut = cut.expect("a result this size has to be cut");
        assert_eq!(cut.tool, "Read");
        assert!(cut.original_bytes > MAX_TOOL_RESULT_BYTES);
        assert!(cut.kept_bytes < cut.original_bytes);

        assert_eq!(payload["truncated"], serde_json::json!(true));
        assert_eq!(payload["tool"], serde_json::json!("Read"));
        assert_eq!(payload["keptBytes"], serde_json::json!(cut.kept_bytes));
        assert_eq!(
            payload["limitBytes"],
            serde_json::json!(MAX_TOOL_RESULT_BYTES)
        );
        assert!(payload["value"]
            .as_str()
            .is_some_and(|kept| kept.starts_with('y')));

        // And what goes on the wire is a *successful* result, because the
        // program has to keep running: an `ok: false` reaches it as a rejected
        // promise, which is how one large file used to end the whole run.
        let frame = ToolResultFrame {
            v: PROTOCOL_VERSION,
            kind: "tool_result",
            id: 1,
            ok: true,
            value: Some(&payload),
            error: None,
        };
        let encoded = encode_parent_frame_within(&frame, MAX_TOOL_RESULT_BYTES)
            .expect("a truncated result fits by construction");
        assert!(encoded.len() <= MAX_TOOL_RESULT_BYTES);
    }

    /// The text is cut on a character boundary, not in the middle of one.
    #[test]
    fn truncation_does_not_split_a_character() {
        let value = serde_json::json!("水".repeat(200));
        let (payload, cut) = tool_result_payload("Read", &value, 512);
        assert!(cut.is_some());
        let kept = payload["value"].as_str().expect("kept text is a string");
        assert!(
            kept.chars().all(|c| c == '水'),
            "no half characters: {kept:?}"
        );
    }

    /// A refusal that survives still has to name the tool and the numbers.
    #[test]
    fn an_oversize_frame_error_says_which_tool_and_how_big() {
        let value = serde_json::json!("z".repeat(4096));
        let frame = ToolResultFrame {
            v: PROTOCOL_VERSION,
            kind: "tool_result",
            id: 1,
            ok: true,
            value: Some(&value),
            error: None,
        };
        let message = encode_parent_frame_within(&frame, 1024).unwrap_err();
        assert!(message.contains("1024"), "the limit is named: {message}");
        assert!(message.contains("bytes"), "the size is named: {message}");
    }

    /// The helper takes both caps from the frame rather than keeping its own.
    #[test]
    fn the_helper_has_no_frame_limit_of_its_own() {
        assert!(
            !NODE_HELPER.contains("const MAX_FRAME_BYTES"),
            "the limit lives in Rust; a second copy is a second number that can drift"
        );
        assert!(NODE_HELPER.contains("request.limits.maxFrameBytes"));
        assert!(NODE_HELPER.contains("request.limits.maxIncomingBytes"));
    }

    #[test]
    fn embedded_helper_is_not_the_plugin_host() {
        assert!(NODE_HELPER.contains("One-shot Code Mode executor"));
        assert!(!NODE_HELPER.contains("plugin-host/src"));
        assert!(!NODE_HELPER.contains("@rebon/plugin-host"));
    }
}
