use rebon_boa_runner::{
    helper_candidates_from_executable, helper_path_from_executable, IsolatedJsRunner, Limits,
    RunnerError, StatusLineScriptError, StatusLineScriptRunner, StatusScriptRequest,
    DEFAULT_CODE_BYTES, DEFAULT_PAYLOAD_BYTES, DEFAULT_REQUEST_BYTES, DEFAULT_RESPONSE_BYTES,
    DEFAULT_STDERR_BYTES, MAX_MEMORY_BYTES, STATUS_LINE_SCRIPT_SOURCE_BYTES,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn runner() -> (IsolatedJsRunner, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("temp directory");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_rebon-boa-test-helper"));
    let runner = IsolatedJsRunner::new(helper, directory.path().canonicalize().unwrap()).unwrap();
    (runner, directory)
}

fn status_runner() -> (StatusLineScriptRunner, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("temp directory");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_rebon-boa-test-helper"));
    let runner = StatusLineScriptRunner::new(helper).unwrap();
    (runner, directory)
}

/// The script rebon ships, which is the one whose cost and output matter.
fn bundled_default_script() -> PathBuf {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("assets")
        .join("statusline-default.js");
    assert!(
        script.is_file(),
        "missing bundled script at {}",
        script.display()
    );
    script
}

/// A payload shaped like a real render: a model, a context window, and usage.
fn bundled_default_payload() -> serde_json::Value {
    json!({
        "surface": "app",
        "placement": "assistant_message",
        "model": {"id": "claude-opus-5", "display_name": "claude-opus-5"},
        "context_window": {
            "context_window_size": 200_000,
            "current_usage": {"input_tokens": 100_000, "output_tokens": 1_500},
            "used_percentage": 50,
            "remaining_percentage": 50
        },
        "last_turn_usage": {
            "output_tokens": 1_500,
            "cache_read_hit_input_tokens": 80_000,
            "cache_write_miss_input_tokens": 20_000,
            "cache_total_input_tokens": 100_000
        },
        "total_usage": {"total_tokens": 500_000, "output_tokens": 25_000}
    })
}

fn request(code: &str) -> StatusScriptRequest {
    StatusScriptRequest {
        code: code.into(),
        payload: json!({"name": "Ada", "nested": {"value": 7}}),
    }
}

#[test]
fn healthy_payload_json_and_logs() {
    let (runner, _directory) = runner();
    let output = runner
        .run(
            request("console.log('render', payload.name); return { text: payload.name, n: payload.nested.value };"),
            Limits::default(),
        )
        .unwrap();
    assert_eq!(output.value, json!({"text": "Ada", "n": 7}));
    assert_eq!(output.logs, vec!["render Ada"]);
}

#[test]
fn errors_cycles_async_and_large_output_are_stable() {
    let (runner, _directory) = runner();
    for code in [
        "throw new Error('expected');",
        "const x = {}; x.x = x; return x;",
        "return Promise.resolve(1);",
        "return 'x'.repeat(20_000);",
    ] {
        let error = runner.run(request(code), Limits::default()).unwrap_err();
        assert!(
            matches!(
                error,
                RunnerError::Script { .. } | RunnerError::OutputTooLarge { .. }
            ),
            "{error:?}"
        );
        assert!(!error.to_string().contains("payload.nested"));
    }
}

#[test]
fn status_adapter_preserves_payload_terminal_logs_and_module_forms() {
    let (runner, directory) = status_runner();
    let output = runner
        .run_source(
            "// The module uses export default below.\nexport default function render(payload) { console.log('render', payload.name); return payload.name + '|' + payload.nested.value + '|' + payload.terminal.columns + ':' + payload.terminal.lines; }",
            json!({"name": "Ada", "nested": {"value": 7}}),
            directory.path(),
            91,
            13,
            None,
        )
        .unwrap();
    assert_eq!(output.lines, vec!["Ada|7|91:13"]);
    assert_eq!(output.logs, vec!["render Ada"]);

    let named = runner
        .run_source(
            "export function render(payload) { return payload.surface === 'app' ? '{icon:bot} app' : 'terminal'; }",
            json!({"surface": "app"}),
            directory.path(),
            80,
            24,
            None,
        )
        .unwrap();
    assert_eq!(named.lines, vec!["{icon:bot} app"]);
}

#[test]
fn bundled_default_script_keeps_app_icons_and_terminal_text() {
    let (runner, directory) = status_runner();
    let script = bundled_default_script();
    let payload = bundled_default_payload();
    let app = runner
        .run_file(&script, payload.clone(), directory.path(), 120, 24, None)
        .unwrap();
    assert_eq!(app.lines.len(), 1);
    assert!(app.lines[0].contains("{icon:bot}"));
    assert!(app.lines[0].contains("CLAUDE OPUS 5"));
    assert!(app.lines[0].contains("{icon:bolt}"));

    let mut terminal_payload = payload;
    terminal_payload["surface"] = json!("tui");
    let terminal = runner
        .run_file(&script, terminal_payload, directory.path(), 120, 24, None)
        .unwrap();
    assert_eq!(terminal.lines.len(), 1);
    assert!(!terminal.lines[0].contains("{icon:"));
    assert!(terminal.lines[0].contains('●'));
}

/// The bundled script, run the way a status line runs it.
///
/// A status line is not a batch job: it renders on a debounce measured in
/// hundreds of milliseconds, and a render that costs more than that stops being
/// a status line and becomes a stutter. Isolating the script in a process buys
/// the hard termination this runner exists for, and the price of that isolation
/// is a spawn per render — so the price is what this measures.
///
/// The bound is deliberately loose against the 250 ms debounce both surfaces
/// use: this is a regression fence, not a benchmark, and a debug build on a
/// loaded machine is the slowest this will ever be. What it catches is the
/// change that makes a render cost seconds.
#[test]
fn a_status_render_costs_a_fraction_of_its_debounce() {
    const RENDERS: u32 = 10;
    // The debounce both the terminal and the desktop app hold a status line to.
    const DEBOUNCE: Duration = Duration::from_millis(250);

    let (runner, directory) = status_runner();
    let script = bundled_default_script();
    let payload = bundled_default_payload();

    // One warm run first: the fence is about steady-state renders, and the
    // first one on a cold page cache measures the filesystem.
    runner
        .run_file(&script, payload.clone(), directory.path(), 120, 24, None)
        .expect("the bundled script renders");

    let started = Instant::now();
    for _ in 0..RENDERS {
        let output = runner
            .run_file(&script, payload.clone(), directory.path(), 120, 24, None)
            .expect("the bundled script renders");
        assert_eq!(output.lines.len(), 1);
    }
    let each = started.elapsed() / RENDERS;
    assert!(
        each < DEBOUNCE,
        "a status render took {each:?}, which is more than the {DEBOUNCE:?} debounce it has to fit inside"
    );
}

#[test]
fn status_adapter_exposes_no_host_primitives() {
    let (runner, directory) = status_runner();
    let output = runner
        .run_source(
            "export default function render() { return [typeof process, typeof require, typeof fetch, typeof WebSocket, typeof Deno].join(','); }",
            json!({}),
            directory.path(),
            80,
            24,
            None,
        )
        .unwrap();
    assert_eq!(
        output.lines,
        vec!["undefined,undefined,undefined,undefined,undefined"]
    );
}

#[test]
fn status_adapter_rejects_errors_promises_non_strings_and_empty_results() {
    let (runner, directory) = status_runner();
    let cases = [
        (
            "export default function render() { throw new Error('broken'); }",
            "script",
        ),
        (
            "export default function render(payload) { payload.nested.value = 99; return 'mutated'; }",
            "script",
        ),
        (
            "export default function render() { return Promise.resolve('late'); }",
            "script",
        ),
        (
            "export default function render() { return {text: 'no'}; }",
            "non-string",
        ),
        (
            "export default function render() { return '  \\n'; }",
            "empty",
        ),
    ];
    for (source, expected) in cases {
        let error = runner
            .run_source(source, json!({}), directory.path(), 80, 24, None)
            .unwrap_err();
        match expected {
            "script" => assert!(
                matches!(
                    error,
                    StatusLineScriptError::Runner(RunnerError::Script { .. })
                ),
                "{error:?}"
            ),
            "non-string" => assert!(matches!(error, StatusLineScriptError::NonStringResult)),
            "empty" => assert!(matches!(error, StatusLineScriptError::Empty)),
            _ => unreachable!(),
        }
    }
}

#[test]
fn status_adapter_enforces_source_and_response_caps() {
    let (runner, directory) = status_runner();
    let oversized_source = "x".repeat(STATUS_LINE_SCRIPT_SOURCE_BYTES + 1);
    assert!(matches!(
        runner.run_source(&oversized_source, json!({}), directory.path(), 80, 24, None),
        Err(StatusLineScriptError::SourceTooLarge { .. })
    ));

    let error = runner
        .run_source(
            "export default function render() { return 'x'.repeat(20_000); }",
            json!({}),
            directory.path(),
            80,
            24,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        StatusLineScriptError::Runner(RunnerError::OutputTooLarge { .. })
    ));
}

#[test]
fn status_adapter_timeout_and_cancel_reap_the_helper() {
    let (runner, directory) = status_runner();
    let timeout = runner
        .run_source(
            "export default function render() { for (;;) {} }",
            json!({}),
            directory.path(),
            80,
            24,
            None,
        )
        .unwrap_err();
    let pid = match timeout {
        StatusLineScriptError::Runner(RunnerError::Timeout { child_pid }) => child_pid,
        other => panic!("expected timeout, got {other:?}"),
    };
    assert_process_gone(pid);

    let cancelled = AtomicBool::new(false);
    let cancel = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(25));
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        });
        runner.run_source(
            "export default function render() { for (;;) {} }",
            json!({}),
            directory.path(),
            80,
            24,
            Some(&cancelled),
        )
    })
    .unwrap_err();
    let pid = match cancel {
        StatusLineScriptError::Runner(RunnerError::Cancelled { child_pid }) => child_pid,
        other => panic!("expected cancellation, got {other:?}"),
    };
    assert_process_gone(pid);
}

#[test]
fn status_adapter_reports_missing_helpers_and_repeats_without_leaks() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory
        .path()
        .join(if cfg!(windows) { "rebon.exe" } else { "rebon" });
    let candidates = helper_candidates_from_executable(&executable);
    assert!(!candidates.is_empty());
    assert!(matches!(
        helper_path_from_executable(&executable),
        Err(StatusLineScriptError::HelperNotFound { searched }) if searched == candidates
    ));

    let missing = directory.path().join(if cfg!(windows) {
        "missing-helper.exe"
    } else {
        "missing-helper"
    });
    let missing_runner = StatusLineScriptRunner::new(missing).unwrap();
    assert!(matches!(
        missing_runner.run_source(
            "export default function render() { return 'never'; }",
            json!({}),
            directory.path(),
            80,
            24,
            None
        ),
        Err(StatusLineScriptError::Runner(RunnerError::Io(_)))
            | Err(StatusLineScriptError::Runner(
                RunnerError::ContainmentSetup(_)
            ))
    ));

    let (runner, run_directory) = status_runner();
    for index in 0..20 {
        let output = runner
            .run_source(
                "export default function render(payload) { return 'run-' + payload.index; }",
                json!({"index": index}),
                run_directory.path(),
                80,
                24,
                None,
            )
            .unwrap();
        assert_eq!(output.lines, vec![format!("run-{index}")]);
    }
}

#[test]
fn script_error_text_cannot_impersonate_an_output_overflow() {
    let (runner, _directory) = runner();
    assert!(matches!(
        runner.run(
            request("throw 'response exceeds helper limit';"),
            Limits::default()
        ),
        Err(RunnerError::Script { .. })
    ));
}

#[test]
fn promise_brand_and_log_storage_resist_lexical_and_global_tampering() {
    let (runner, _directory) = runner();
    let promise_error = runner
        .run(
            request(
                "const genuine = Promise.resolve(1); globalThis.Promise = function Fake() {}; return genuine;",
            ),
            Limits::default(),
        )
        .unwrap_err();
    assert!(matches!(promise_error, RunnerError::Script { .. }));
    assert!(promise_error
        .to_string()
        .contains("Promise results are not supported"));

    let output = runner
        .run(
            request(
                "globalThis.__logs = []; __logs.push('forged'); console.log('real'); return __logs.length;",
            ),
            Limits::default(),
        )
        .unwrap();
    assert_eq!(output.value, json!(1));
    assert_eq!(output.logs, vec!["real"]);

    let serialized = runner
        .run(
            request(
                "globalThis.JSON.stringify = () => '\"forged\"'; console.log({safe: 1}); return {safe: 2};",
            ),
            Limits::default(),
        )
        .unwrap();
    assert_eq!(serialized.value, json!({"safe": 2}));
    assert_eq!(serialized.logs, vec![r#"{"safe":1}"#]);

    let direct_reference = runner
        .run(
            request("__logs.push('forged'); return 1;"),
            Limits::default(),
        )
        .unwrap_err();
    assert!(matches!(direct_reference, RunnerError::Script { .. }));
}

#[test]
fn code_payload_and_encoded_request_caps_fail_before_spawn() {
    let (runner, _directory) = runner();
    let code_limits = Limits {
        code_bytes: 8,
        ..Limits::default()
    };
    assert!(matches!(
        runner.run(request("return 123456789;"), code_limits),
        Err(RunnerError::InputTooLarge { limit: 8 })
    ));

    let payload_limits = Limits {
        payload_bytes: 4,
        ..Limits::default()
    };
    assert!(matches!(
        runner.run(request("return 1;"), payload_limits),
        Err(RunnerError::InputTooLarge { limit: 4 })
    ));

    let request_limits = Limits {
        request_bytes: 16,
        ..Limits::default()
    };
    assert!(matches!(
        runner.run(request("return 1;"), request_limits),
        Err(RunnerError::InputTooLarge { limit: 16 })
    ));
}

#[test]
fn invalid_public_limits_have_one_pre_spawn_error_class() {
    let (runner, _directory) = runner();
    let invalid = [
        Limits {
            wall_time: Duration::ZERO,
            ..Limits::default()
        },
        Limits {
            wall_time: Duration::MAX,
            ..Limits::default()
        },
        Limits {
            memory_bytes: 64 * 1024 * 1024 - 1,
            ..Limits::default()
        },
        Limits {
            memory_bytes: MAX_MEMORY_BYTES + 1,
            ..Limits::default()
        },
        Limits {
            memory_bytes: u64::MAX,
            ..Limits::default()
        },
        Limits {
            request_bytes: DEFAULT_REQUEST_BYTES + 1,
            ..Limits::default()
        },
        Limits {
            response_bytes: DEFAULT_RESPONSE_BYTES + 1,
            ..Limits::default()
        },
        Limits {
            code_bytes: DEFAULT_CODE_BYTES + 1,
            ..Limits::default()
        },
        Limits {
            payload_bytes: DEFAULT_PAYLOAD_BYTES + 1,
            ..Limits::default()
        },
    ];
    for limits in invalid {
        assert!(matches!(
            runner.run(request("return 1;"), limits),
            Err(RunnerError::InvalidLimits(_))
        ));
    }
}

#[test]
fn finite_memory_maximum_accepts_its_boundary() {
    let (runner, _directory) = runner();
    let output = runner
        .run(
            request("return 9;"),
            Limits {
                memory_bytes: MAX_MEMORY_BYTES,
                ..Limits::default()
            },
        )
        .unwrap();
    assert_eq!(output.value, json!(9));
}

#[cfg(unix)]
#[test]
fn unix_no_limit_sentinel_is_rejected() {
    let (runner, _directory) = runner();
    let error = runner
        .run(
            request("return 1;"),
            Limits {
                memory_bytes: libc::RLIM_INFINITY as u64,
                ..Limits::default()
            },
        )
        .unwrap_err();
    assert!(matches!(error, RunnerError::InvalidLimits(_)));
}

#[test]
fn tiny_response_caps_report_the_exact_logical_cap() {
    let (runner, _directory) = runner();
    for response_bytes in [0, 1] {
        assert!(matches!(
            runner.run(
                request("return 1;"),
                Limits {
                    response_bytes,
                    ..Limits::default()
                }
            ),
            Err(RunnerError::OutputTooLarge { limit }) if limit == response_bytes
        ));
    }
}

#[test]
fn stderr_limit_is_always_normalized_to_discard() {
    assert_eq!(DEFAULT_STDERR_BYTES, 0);
    let (runner, _directory) = runner();
    for stderr_bytes in [0, usize::MAX] {
        assert_eq!(
            runner
                .run(
                    request("return 7;"),
                    Limits {
                        stderr_bytes,
                        ..Limits::default()
                    }
                )
                .unwrap()
                .value,
            json!(7)
        );
    }
}

#[test]
fn async_promise_and_module_syntax_are_explicitly_unsupported() {
    let (runner, _directory) = runner();
    for code in [
        "return Promise.resolve(1);",
        "return (async function () { return 1; })();",
        "return import('not-a-module');",
        "await Promise.resolve(1); return 1;",
        "import value from 'not-a-module'; return value;",
        "export default 1;",
    ] {
        assert!(matches!(
            runner.run(request(code), Limits::default()),
            Err(RunnerError::Script { .. })
        ));
    }
}

#[test]
fn pre_cancel_is_fail_fast_without_claiming_a_reaped_child() {
    let (runner, _directory) = runner();
    let cancelled = AtomicBool::new(true);
    let error = runner
        .run_with_cancel(request("for (;;) {}"), Limits::default(), &cancelled)
        .unwrap_err();
    assert!(matches!(&error, RunnerError::CancelledBeforeStart));
    assert_eq!(
        error.to_string(),
        "script execution was cancelled before child start"
    );
}

#[test]
fn infinite_loop_is_killed_and_reaped_twenty_times() {
    let (runner, _directory) = runner();
    let limits = Limits {
        wall_time: Duration::from_millis(80),
        ..Limits::default()
    };
    let started = Instant::now();
    for _ in 0..20 {
        let error = runner
            .run(request("for (;;) {}"), limits.clone())
            .unwrap_err();
        let pid = match error {
            RunnerError::Timeout { child_pid } => child_pid,
            other => panic!("unexpected error: {other:?}"),
        };
        assert_process_gone(pid);
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        runner
            .run(request("return 42;"), Limits::default())
            .unwrap()
            .value,
        json!(42)
    );
}

#[cfg(all(windows, target_pointer_width = "32"))]
#[test]
fn windows_rejects_memory_caps_that_do_not_fit_usize() {
    let (runner, _directory) = runner();
    let error = runner
        .run(
            request("return 1;"),
            Limits {
                memory_bytes: u64::from(u32::MAX) + 1,
                ..Limits::default()
            },
        )
        .unwrap_err();
    assert!(matches!(error, RunnerError::InvalidLimits(_)));
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
        assert_eq!(wait, WAIT_OBJECT_0, "child PID {pid} is still running");
    }
}

#[cfg(unix)]
fn assert_process_gone(pid: u32) {
    let result = unsafe { libc::kill(pid as i32, 0) };
    assert_eq!(result, -1, "child PID {pid} still exists");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
