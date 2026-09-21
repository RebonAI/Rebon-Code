use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use rebon_code_runner::{
    active_code_executions, run_code_program, run_code_program_with_runtime, CodeNodeRuntime,
    CodeToolDispatcher,
};
use serde_json::{json, Value};

const NODE_HELPER: &str = include_str!("../js/code_mode_runner.js");

fn test_lock() -> &'static tokio::sync::Mutex<()> {
    static TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    TEST_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Default)]
struct ProbeDispatcher {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

struct ActiveDispatch(Arc<AtomicUsize>);

impl Drop for ActiveDispatch {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl CodeToolDispatcher for ProbeDispatcher {
    fn dispatch(&self, name: String, input: Value) -> BoxFuture<'static, Result<Value, String>> {
        let delay = input.get("delay").and_then(Value::as_u64).unwrap_or(0);
        let value = input.get("value").cloned().unwrap_or(input);
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        self.calls.fetch_add(1, Ordering::AcqRel);
        let active_guard = ActiveDispatch(self.active.clone());
        Box::pin(async move {
            let _active = active_guard;
            if delay != 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if name == "reject" {
                Err("probe rejected".into())
            } else {
                Ok(value)
            }
        })
    }
}

fn runtime() -> CodeNodeRuntime {
    let output = std::process::Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .expect("Node is required for Code Mode tests");
    assert!(output.status.success());
    let path = String::from_utf8(output.stdout).unwrap().trim().to_string();
    CodeNodeRuntime::new(PathBuf::from(path)).unwrap()
}

fn helper_failure(request: &[u8]) -> Value {
    use std::io::Write as _;

    let node = runtime();
    let mut child = std::process::Command::new(node.executable())
        .args(["--no-warnings", "-e", NODE_HELPER])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(request).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    serde_json::from_slice(output.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap()
}

async fn run(
    code: &str,
    dispatcher: Arc<dyn CodeToolDispatcher>,
    budget: Duration,
) -> Result<rebon_code_runner::CodeOutcome, rebon_code_runner::CodeProgramError> {
    run_code_program_with_runtime(code.into(), dispatcher, budget, runtime()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_rejects_version_mismatch_corruption_and_oversize_requests() {
    let _guard = test_lock().lock().await;
    let mismatch = helper_failure(
        br#"{"v":999,"type":"execute","code":"return 1","limits":{"maxOutputBytes":1,"maxLogBytes":1,"maxLogLines":1}}
"#,
    );
    assert_eq!(mismatch["kind"], "protocol");
    assert!(mismatch["message"]
        .as_str()
        .unwrap()
        .contains("version mismatch"));

    let corrupt = helper_failure(b"not-json\n");
    assert_eq!(corrupt["kind"], "protocol");
    assert!(corrupt["message"].as_str().unwrap().contains("strict JSON"));

    // Over the bootstrap the helper reads with before it has been told the real
    // caps. That threshold moved with the larger tool-result cap: a tool result
    // may now be far larger than one frame used to be, so the handshake
    // allowance moved with it and this line has to be past the new one to be
    // refused at all.
    let mut oversize = vec![b'x'; 2 * 1024 * 1024 + 1];
    oversize.push(b'\n');
    let oversize = helper_failure(&oversize);
    assert_eq!(oversize["kind"], "protocol");
    assert!(oversize["message"]
        .as_str()
        .unwrap()
        .contains("frame exceeds"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_body_promises_tool_bridge_logs_and_json_return_are_compatible() {
    let _guard = test_lock().lock().await;
    let dispatcher = Arc::new(ProbeDispatcher::default());
    let outcome = run(
        r#"
const values = await Promise.all([
  tools.echo({ value: 2, delay: 30 }),
  tools.invoke("echo", { value: 3, delay: 30 }),
]);
await Promise.resolve();
console.log("values", values);
return { sum: values[0] + values[1] };
"#,
        dispatcher.clone(),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(outcome.result, Some(json!({"sum": 5})));
    assert_eq!(outcome.logs, vec!["values [2,3]"]);
    assert_eq!(dispatcher.calls.load(Ordering::Acquire), 2);
    assert!(dispatcher.peak.load(Ordering::Acquire) >= 2);

    let no_value = run(
        "await Promise.resolve();",
        Arc::new(ProbeDispatcher::default()),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(no_value.result, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launcher_runtime_contract_drives_production_discovery() {
    let _guard = test_lock().lock().await;
    let node = runtime();
    let previous = std::env::var_os("REBON_CODE_MODE_NODE");
    std::env::set_var("REBON_CODE_MODE_NODE", node.executable());
    let outcome = run_code_program(
        "return 42;".into(),
        Arc::new(ProbeDispatcher::default()),
        Duration::from_secs(3),
    )
    .await;
    match previous {
        Some(value) => std::env::set_var("REBON_CODE_MODE_NODE", value),
        None => std::env::remove_var("REBON_CODE_MODE_NODE"),
    }
    assert_eq!(outcome.unwrap().result, Some(json!(42)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejection_syntax_runtime_and_module_policies_are_deterministic() {
    let _guard = test_lock().lock().await;
    for (code, expected) in [
        (
            "await Promise.reject(new Error('promise boom'));",
            "promise boom",
        ),
        ("throw new Error('runtime boom');", "runtime boom"),
        ("return import('node:fs');", "dynamic import is disabled"),
        ("import fs from 'node:fs';", "syntax"),
        ("return NaN;", "not lossless JSON"),
        ("return 1n;", "not lossless JSON"),
        ("return { missing: undefined };", "not lossless JSON"),
        (
            "const value = {}; value.self = value; return value;",
            "not lossless JSON",
        ),
    ] {
        let error = run(
            code,
            Arc::new(ProbeDispatcher::default()),
            Duration::from_secs(3),
        )
        .await
        .unwrap_err();
        assert!(error.message.contains(expected), "{}", error.message);
    }

    let caught = run(
        r#"
const messages = [];
try { await tools.reject({}); } catch (error) { messages.push(error.message); }
try { await tools.echo({ missing: undefined }); } catch (error) { messages.push(error.message); }
return messages;
"#,
        Arc::new(ProbeDispatcher::default()),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(
        caught.result,
        Some(json!([
            "probe rejected",
            "tool input is not lossless JSON: unsupported undefined value"
        ]))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_byte_limit_accepts_64_kib_and_rejects_the_next_byte_before_execution() {
    let _guard = test_lock().lock().await;
    let dispatcher = Arc::new(ProbeDispatcher::default());
    let prefix = "return await tools.echo({ value: 1 });";
    let exact = format!("{prefix}{}", " ".repeat(64 * 1024 - prefix.len()));
    assert_eq!(exact.len(), 64 * 1024);

    let outcome = run(&exact, dispatcher.clone(), Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(outcome.result, Some(json!(1)));
    assert_eq!(dispatcher.calls.load(Ordering::Acquire), 1);

    let over = format!("{exact} ");
    assert_eq!(over.len(), 64 * 1024 + 1);
    let error = run(&over, dispatcher.clone(), Duration::from_secs(3))
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .contains("program exceeds the 65536-byte Code Mode source limit"),
        "{}",
        error.message
    );
    assert_eq!(dispatcher.calls.load(Ordering::Acquire), 1);
    assert_eq!(active_code_executions(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_limit_accepts_2048_and_rejects_the_2049th() {
    let _guard = test_lock().lock().await;
    let dispatcher = Arc::new(ProbeDispatcher::default());
    let outcome = run(
        r#"
let completed = 0;
for (let i = 0; i < 2048; i++) {
  const value = await tools.echo({ value: null });
  if (value !== null) throw new Error("unexpected tool result");
  completed++;
}
let rejection = "allowed";
try { await tools.echo({ value: null }); }
catch (error) { rejection = error.message; }
return { completed, rejection };
"#,
        dispatcher.clone(),
        Duration::from_secs(15),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.result,
        Some(json!({
            "completed": 2048,
            "rejection": "Code Mode tool-call count exceeds the hard limit"
        }))
    );
    assert_eq!(dispatcher.calls.load(Ordering::Acquire), 2048);
    assert_eq!(active_code_executions(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_io_process_modules_and_generated_code_are_denied() {
    let _guard = test_lock().lock().await;
    let outcome = run(
        r#"
let generated;
try { Function("return process")(); generated = "allowed"; }
catch (error) { generated = error.message; }
const escapes = [];
const testErrorConstructor = (error) => {
  try { error.constructor.constructor("return process")(); escapes.push("allowed"); }
  catch (escapeError) { escapes.push(escapeError.message); }
};
for (const candidate of [tools.invoke, console.log]) {
  try { candidate.constructor("return process")(); escapes.push("allowed"); }
  catch (error) { escapes.push(error.message); }
}
const toolValue = await tools.echo({ value: { nested: true } });
try { toolValue.constructor.constructor("return process")(); escapes.push("allowed"); }
catch (error) { escapes.push(error.message); }
try { await tools.reject({}); }
catch (toolError) { testErrorConstructor(toolError); }
let logLimit = "allowed";
try {
  for (let i = 0; i < 1025; i++) console.log("x");
} catch (error) {
  logLimit = error.message;
  testErrorConstructor(error);
}
let dynamicImport = "allowed";
try { await import("node:fs"); }
catch (error) {
  dynamicImport = error.message;
  testErrorConstructor(error);
}
let wasm = "allowed";
try {
  await WebAssembly.compile(new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]));
} catch (error) {
  wasm = error.message;
}
return {
  process: typeof process,
  require: typeof require,
  fetch: typeof fetch,
  module: typeof module,
  generated,
  logLimit,
  dynamicImport,
  wasm,
  escapes,
};
"#,
        Arc::new(ProbeDispatcher::default()),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    let value = outcome.result.unwrap();
    assert_eq!(value["process"], "undefined");
    assert_eq!(value["require"], "undefined");
    assert_eq!(value["fetch"], "undefined");
    assert_eq!(value["module"], "undefined");
    assert!(value["generated"]
        .as_str()
        .unwrap()
        .contains("Code generation from strings disallowed"));
    assert!(value["logLimit"]
        .as_str()
        .unwrap()
        .contains("Code Mode log output exceeds the hard limit"));
    assert!(value["dynamicImport"]
        .as_str()
        .unwrap()
        .contains("dynamic import is disabled in Code Mode"));
    assert!(value["wasm"]
        .as_str()
        .unwrap()
        .contains("Wasm code generation disallowed"));
    let escapes = value["escapes"].as_array().unwrap();
    assert_eq!(escapes.len(), 6);
    assert!(escapes.iter().all(|message| {
        message
            .as_str()
            .is_some_and(|message| message.contains("Code generation from strings disallowed"))
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_log_and_stack_limits_fail_while_memory_pressure_terminates_boundedly() {
    let _guard = test_lock().lock().await;
    let dispatcher: Arc<dyn CodeToolDispatcher> = Arc::new(ProbeDispatcher::default());
    for (code, expected) in [
        (
            "for (let i = 0; i < 1100; i++) console.log('line', i);",
            "log output exceeds",
        ),
        ("return 'x'.repeat(600000);", "output_limit"),
        (
            "function recurse() { return recurse(); } recurse();",
            "Maximum call stack",
        ),
    ] {
        let error = run(code, dispatcher.clone(), Duration::from_secs(4))
            .await
            .unwrap_err();
        assert!(error.message.contains(expected), "{}", error.message);
    }

    let memory = run(
        r#"
const chunks = [];
for (;;) {
  const chunk = new Uint8Array(16 * 1024 * 1024);
  for (let i = 0; i < chunk.length; i += 4096) chunk[i] = 1;
  chunks.push(chunk);
}
"#,
        dispatcher,
        Duration::from_secs(8),
    )
    .await
    .unwrap_err();
    assert!(
        memory.message.contains("resource limit")
            || memory.message.contains("budget")
            || memory.message.contains("heap")
            || memory.message.contains("allocation failed"),
        "{}",
        memory.message
    );
    assert_eq!(active_code_executions(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeouts_never_resolving_promises_and_cancellation_reap_without_leaks() {
    let _guard = test_lock().lock().await;
    for code in [
        "for (;;) {}",
        "await new Promise(() => {});",
        "await tools.echo({ delay: 5000 });",
    ] {
        for _ in 0..5 {
            let dispatcher = Arc::new(ProbeDispatcher::default());
            let error = run(code, dispatcher.clone(), Duration::from_millis(180))
                .await
                .unwrap_err();
            assert!(
                error.message.contains("killed and reaped"),
                "{}",
                error.message
            );
            let pid = reaped_pid(&error.message).expect("timeout includes child pid");
            assert_process_gone(pid);
            assert_eq!(dispatcher.active.load(Ordering::Acquire), 0);
            assert_eq!(active_code_executions(), 0);
        }
    }

    for _ in 0..5 {
        let task = tokio::spawn(run(
            "await new Promise(() => {});",
            Arc::new(ProbeDispatcher::default()),
            Duration::from_secs(20),
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while active_code_executions() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(active_code_executions(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(active_code_executions(), 0);
    }
}

/// The hardened flag set is checked against the runtime that will run the
/// program, not against a version table.
///
/// Node renamed the permission flag when the model went stable, and a hard-coded
/// spelling meant every Code Mode process died at `bad option` on Node 24 — the
/// version rebon pins and installs — while the suite stayed green because it
/// takes whichever Node is on `PATH`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_runtime_is_probed_for_the_flag_set_it_will_be_run_with() {
    let _guard = test_lock().lock().await;
    let node = runtime();
    assert!(
        ["--permission", "--experimental-permission"].contains(&node.permission_flag()),
        "unexpected permission flag {}",
        node.permission_flag()
    );

    // The flag set the probe accepted is the set a program actually runs under,
    // which is what a probe-only test misses: that one never failed on the
    // machine that wrote it, and it fails on every Node that renamed the flag.
    let outcome = run_code_program_with_runtime(
        "return 1 + 1;".into(),
        Arc::new(ProbeDispatcher::default()),
        Duration::from_secs(5),
        node.clone(),
    )
    .await
    .expect("a program runs on the probed runtime");
    assert_eq!(outcome.result, Some(json!(2)));

    // And the flag is not merely accepted, it is in force. The guest itself
    // cannot demonstrate this — it has no modules at all, which is the stronger
    // containment asserted elsewhere — so the permission model is checked from
    // outside, on the same runtime with the same flag, where `require` exists.
    let denied = std::process::Command::new(node.executable())
        .args([node.permission_flag(), "-e"])
        .arg(
            "try { require('node:fs').readFileSync(process.argv[0]); console.log('READ'); } \
             catch (error) { console.log(error.code ?? 'THREW'); }",
        )
        .output()
        .expect("the runtime starts");
    assert_eq!(
        String::from_utf8_lossy(&denied.stdout).trim(),
        "ERR_ACCESS_DENIED",
        "the permission model is not in force under {}",
        node.permission_flag()
    );
}

/// A runtime that cannot be hardened is refused where it is chosen.
///
/// Fail-closed is the whole point: Code Mode runs code the model wrote, so
/// "the sandbox flag was not accepted" must end the attempt rather than start an
/// unsandboxed process. The test binary stands in for any executable that is not
/// a Node new enough to have a permission model — it rejects the flags exactly
/// as such a runtime would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_that_rejects_the_hardening_flags_is_refused_not_used_unsandboxed() {
    let _guard = test_lock().lock().await;
    let not_node = std::env::current_exe().expect("the test binary exists");
    let error = CodeNodeRuntime::new(not_node).expect_err("an unhardenable runtime is refused");
    assert!(
        error
            .message
            .contains("refused rather than used unsandboxed"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("--permission")
            && error.message.contains("--experimental-permission"),
        "the refusal names what it tried: {}",
        error.message
    );
}

/// Isolation costs a process per program, and this is what that costs.
///
/// Measured against the right scale: Code Mode exists to replace N model
/// round-trips with one program, and a round-trip is seconds. Process startup
/// only has to be small next to that — it is roughly 200 ms on the pinned Node,
/// against a 120 s program budget. The bounds below are a regression fence
/// against a start that costs seconds before a program runs, not a benchmark.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_cold_and_cached_startup_fit_code_mode_budget() {
    let _guard = test_lock().lock().await;
    let cold_started = Instant::now();
    run(
        "return 1;",
        Arc::new(ProbeDispatcher::default()),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert!(cold_started.elapsed() < Duration::from_secs(2));

    let warm_started = Instant::now();
    for _ in 0..5 {
        run(
            "return 1;",
            Arc::new(ProbeDispatcher::default()),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
    }
    assert!(warm_started.elapsed() < Duration::from_secs(4));
    assert_eq!(active_code_executions(), 0);
}

fn reaped_pid(message: &str) -> Option<u32> {
    let rest = message.split("execution unit ").nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

#[cfg(windows)]
fn assert_process_gone(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS,
            0,
            pid,
        )
    };
    if handle != 0 {
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        unsafe { CloseHandle(handle) };
        assert_eq!(wait, WAIT_OBJECT_0, "Code Mode child {pid} survived");
    }
}

#[cfg(unix)]
fn assert_process_gone(pid: u32) {
    let result = unsafe { libc::kill(pid as i32, 0) };
    assert_eq!(result, -1, "Code Mode child {pid} survived");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

/// A tool result of whatever size the program asks for, through a real child.
struct BigDispatcher;

impl CodeToolDispatcher for BigDispatcher {
    fn dispatch(&self, _name: String, input: Value) -> BoxFuture<'static, Result<Value, String>> {
        let bytes = input.get("bytes").and_then(Value::as_u64).unwrap_or(0) as usize;
        Box::pin(async move { Ok(Value::String("x".repeat(bytes))) })
    }
}

/// The past failure this guards: a program read one ordinary file and the whole
/// run failed with `tool result exceeds the Code Mode frame limit`.
///
/// 1.5 MiB is over the old frame limit and under the new tool-result one, so
/// the program now gets the whole thing and runs to its own end. Through a real
/// Node child, because the failure was in what crossed between the processes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_result_over_the_old_frame_limit_no_longer_ends_the_program() {
    let _guard = test_lock().lock().await;
    let outcome = run(
        "const text = await tools.Read({ bytes: 1572864 });\n\
         return `read ${text.length} chars`;",
        Arc::new(BigDispatcher),
        Duration::from_secs(20),
    )
    .await
    .expect("the program runs to completion");
    assert_eq!(outcome.result, Some(json!("read 1572864 chars")));
}

/// Past the new limit it is shortened rather than refused, the program still
/// finishes, and the answer says what was cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_past_the_new_limit_arrives_truncated_and_the_run_says_so() {
    let _guard = test_lock().lock().await;
    let outcome = run(
        "const value = await tools.Read({ bytes: 9000000 });\n\
         return value && value.truncated === true\n\
           ? `truncated ${value.originalBytes} -> ${value.keptBytes}`\n\
           : `unexpected ${typeof value}`;",
        Arc::new(BigDispatcher),
        Duration::from_secs(30),
    )
    .await
    .expect("a result too large to pass whole must not end the program");

    let text = match outcome.result {
        Some(Value::String(text)) => text,
        other => panic!("expected the program's own summary, got {other:?}"),
    };
    assert!(
        text.starts_with("truncated "),
        "the program saw the marker: {text}"
    );
    assert!(
        outcome
            .logs
            .iter()
            .any(|line| line.contains("[truncated] Read returned")),
        "the run tells the reader what was cut: {:?}",
        outcome.logs
    );
}
