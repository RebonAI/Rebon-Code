//! PTC / Code Mode acceptance: the model writes ONE
//! program that orchestrates a chain of tool calls — sequential reads,
//! parallel dispatch, intermediate processing — and every nested call runs
//! the full engine pipeline (permissions included). The transport itself
//! has no ambient I/O: a sandboxed isolate whose only doorway is the
//! dispatch op.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rebon_code_runner::CodeNodeRuntime;
use rebon_core::Engine;
use rebon_kernel::Kernel;
use rebon_kernel_seats::kernel_code_mode::RunCodeTool;
use rebon_kernel_seats::kernel_services::bootstrap_session_context_with_tools;
use rebon_tool::{PermissionBroker, Tool, ToolContext};
use rebon_tools_core::{
    PermissionBehavior, PermissionDecision, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};

#[derive(Default)]
struct RecordingBroker {
    asks: Mutex<Vec<String>>,
}

#[async_trait]
impl PermissionBroker for RecordingBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        if matches!(decision.behavior, PermissionBehavior::Ask) {
            self.asks
                .lock()
                .unwrap()
                .push(tool.id().as_str().to_string());
        }
        let input = decision.updated_input.unwrap_or(input);
        tool.call(input, context).await
    }
}

fn engine_with_builtin_tools() -> Arc<Engine> {
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(rebon_tool::AgentRegistry::load_with_plugin_dirs(
        dir.path(),
        &dir.path().join("config"),
        &[],
    ));
    rebon_tool::set_agent_registry_selection(registry, false);
    Arc::new(Engine::with_builtin_tools())
}

struct ConcurrencyProbe {
    active: AtomicUsize,
    peak: Arc<AtomicUsize>,
}

struct ActiveProbe<'a>(&'a AtomicUsize);

impl Drop for ActiveProbe<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait]
impl Tool for ConcurrencyProbe {
    fn id(&self) -> ToolId {
        ToolId::new("ConcurrencyProbe")
    }

    fn description(&self) -> &str {
        "test nested Code Mode concurrency"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({"type": "object", "additionalProperties": true})
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        let _active = ActiveProbe(&self.active);
        tokio::time::sleep(Duration::from_millis(75)).await;
        Ok(input)
    }
}

fn engine_with_concurrency_probe() -> (Arc<Engine>, Arc<AtomicUsize>) {
    let peak = Arc::new(AtomicUsize::new(0));
    let mut engine = Engine::new();
    engine.register_tool(Arc::new(ConcurrencyProbe {
        active: AtomicUsize::new(0),
        peak: peak.clone(),
    }));
    (Arc::new(engine), peak)
}

fn code_node_runtime() -> CodeNodeRuntime {
    let output = std::process::Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .expect("Node is required for Code Mode tests");
    assert!(output.status.success(), "Node runtime probe failed");
    let path = String::from_utf8(output.stdout)
        .expect("Node path is UTF-8")
        .trim()
        .to_string();
    CodeNodeRuntime::new(std::path::PathBuf::from(path)).expect("absolute Node runtime")
}

fn install_code_runtime(
    tools: &Arc<rebon_kernel_seats::kernel_tool_dispatch::SessionPluginTools>,
    engine: &Arc<Engine>,
    events: &rebon_kernel::Context,
    budget: Duration,
) {
    tools.set_run_code(RunCodeTool::with_budget_and_runtime(
        engine.clone(),
        events.clone(),
        budget,
        code_node_runtime(),
    ));
}

fn slash(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Production resolution reaches a runtime without being launched by npm.
///
/// The launcher rung only exists when an npm-distributed rebon was started by a
/// Node, so before the managed rung a desktop install had no Code Mode at all —
/// even with a vetted runtime already unpacked under its own config home. The
/// assertion is deliberately one-sided: a machine with neither rung is a valid
/// state and must produce a refusal that says how to fix it, not a panic.
#[test]
fn code_mode_resolves_a_runtime_from_the_launcher_or_a_managed_install() {
    match rebon_kernel_seats::kernel_code_mode::code_mode_runtime() {
        Ok(runtime) => {
            assert!(runtime.executable().is_absolute());
            assert!(
                ["--permission", "--experimental-permission"].contains(&runtime.permission_flag()),
                "unexpected permission flag {}",
                runtime.permission_flag()
            );
        }
        Err(error) => {
            // A machine with neither rung must be told what would fix it...
            assert!(
                error.message.contains("rebon node install") || error.message.contains("refused"),
                "{}",
                error.message
            );
            // ...and must actually be such a machine. An installed, vetted
            // runtime that resolution fails to find is the gap this rung exists
            // to close, so it is a failure here rather than a tolerated branch.
            let managed = rebon_node_runtime::ManagedRuntimeStore::under_config_home(
                &rebon_config::config_home_dir(),
            );
            assert!(
                managed.installed().is_empty(),
                "a managed runtime is installed but Code Mode did not reach it: {}",
                error.message
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn one_program_orchestrates_reads_parallel_dispatch_and_logs() {
    let kernel = Kernel::new();
    let engine = engine_with_builtin_tools();
    let (session_ctx, session_tools) =
        bootstrap_session_context_with_tools(&kernel, "sess-ptc", &engine);
    install_code_runtime(
        &session_tools,
        &engine,
        &session_ctx,
        Duration::from_secs(10),
    );

    // Code Mode is model-visible in the session's plugin layer.
    let names = rebon_tool::PluginToolProvider::tool_names(session_tools.as_ref());
    assert!(names.iter().any(|n| n == "run_code"), "{names:?}");

    // Nested-dispatch lifecycle events are observable per sub-call.
    let dispatches: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let dispatches = dispatches.clone();
        session_ctx.on_json("code/dispatch", move |payload| {
            let tool = payload["tool"].as_str().unwrap_or_default().to_string();
            let is_error = payload["isError"].as_bool().unwrap_or(true);
            dispatches.lock().unwrap().push((tool, is_error));
        });
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let file_a = dir.path().join("a.txt");
    let file_b = dir.path().join("b.txt");
    std::fs::write(&file_a, "alpha PTC_MARKER alpha").unwrap();
    std::fs::write(&file_b, "beta beta beta").unwrap();

    let broker = Arc::new(RecordingBroker::default());
    let context = ToolContext::new()
        .with_plugin_tools(session_tools.clone() as Arc<dyn rebon_tool::PluginToolProvider>)
        .with_permission_broker(broker.clone());

    // ONE program: two parallel reads, intermediate processing, logging,
    // and a curated structured return.
    let code = format!(
        r#"
const [a, b] = await Promise.all([
  tools.Read({{ file_path: "{a}" }}),
  tools.Read({{ file_path: "{b}" }}),
]);
console.log("both reads finished");
const text = (value) => (typeof value === "string" ? value : JSON.stringify(value));
return {{
  markerInA: text(a).includes("PTC_MARKER"),
  markerInB: text(b).includes("PTC_MARKER"),
}};
"#,
        a = slash(&file_a),
        b = slash(&file_b),
    );
    let out = engine
        .invoke_tool(
            "run_code",
            json!({ "code": code, "description": "Read two files and compare markers" }),
            &context,
        )
        .await
        .expect("program executes");
    let text = out.as_str().expect("rendered text");
    assert!(text.contains("both reads finished"), "{text}");
    assert!(text.contains("\"markerInA\": true"), "{text}");
    assert!(text.contains("\"markerInB\": false"), "{text}");

    // Read-only nested dispatch asked for nothing; both sub-calls settled
    // cleanly and were observable.
    assert!(broker.asks.lock().unwrap().is_empty());
    let settled = dispatches.lock().unwrap().clone();
    assert_eq!(settled.len(), 2, "{settled:?}");
    assert!(settled
        .iter()
        .all(|(tool, is_error)| tool == "Read" && !is_error));
}

#[tokio::test(flavor = "current_thread")]
async fn nested_write_rides_the_full_permission_pipeline() {
    let kernel = Kernel::new();
    let engine = engine_with_builtin_tools();
    let (session_ctx, session_tools) =
        bootstrap_session_context_with_tools(&kernel, "sess-ptc-perm", &engine);
    install_code_runtime(
        &session_tools,
        &engine,
        &session_ctx,
        Duration::from_secs(10),
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("out.txt");
    let broker = Arc::new(RecordingBroker::default());
    let context = ToolContext::new()
        .with_plugin_tools(session_tools.clone() as Arc<dyn rebon_tool::PluginToolProvider>)
        .with_permission_broker(broker.clone());

    let code = format!(
        r#"
await tools.Write({{ file_path: "{p}", content: "written by ptc" }});
return "wrote it";
"#,
        p = slash(&target),
    );
    let out = engine
        .invoke_tool(
            "run_code",
            json!({ "code": code, "description": "Write one file through the pipeline" }),
            &context,
        )
        .await
        .expect("program executes");
    assert_eq!(out.as_str().unwrap(), "wrote it");

    // The nested Write went through Ask — the transport asked nothing, the
    // effectful sub-call did.
    assert_eq!(broker.asks.lock().unwrap().as_slice(), ["Write"]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "written by ptc");
}

#[tokio::test(flavor = "current_thread")]
async fn nested_dispatch_concurrency_is_bounded() {
    let kernel = Kernel::new();
    let (engine, peak) = engine_with_concurrency_probe();
    let (session_ctx, session_tools) =
        bootstrap_session_context_with_tools(&kernel, "sess-ptc-concurrency", &engine);
    install_code_runtime(
        &session_tools,
        &engine,
        &session_ctx,
        Duration::from_secs(10),
    );
    let context = ToolContext::new()
        .with_plugin_tools(session_tools.clone() as Arc<dyn rebon_tool::PluginToolProvider>);

    let out = engine
        .invoke_tool(
            "run_code",
            json!({
                "code": "const values = await Promise.all(Array.from({ length: 25 }, (_, value) => tools.ConcurrencyProbe({ value }))); return values.length;",
                "description": "Probe bounded nested dispatch concurrency",
            }),
            &context,
        )
        .await
        .expect("program executes");

    assert_eq!(out.as_str(), Some("25"));
    assert_eq!(peak.load(Ordering::Acquire), 10);
}

#[tokio::test(flavor = "current_thread")]
async fn failures_are_loud_and_the_transport_cannot_recurse() {
    let kernel = Kernel::new();
    let engine = engine_with_builtin_tools();
    let (session_ctx, session_tools) =
        bootstrap_session_context_with_tools(&kernel, "sess-ptc-fail", &engine);
    install_code_runtime(
        &session_tools,
        &engine,
        &session_ctx,
        Duration::from_secs(10),
    );

    let context = ToolContext::new()
        .with_plugin_tools(session_tools.clone() as Arc<dyn rebon_tool::PluginToolProvider>);

    // Errors from nested dispatch propagate INTO the program as catchable
    // exceptions; run_code refuses to dispatch itself.
    let code = r#"
const messages = [];
try { await tools.NoSuchTool({}); } catch (e) { messages.push(e.message); }
try { await tools.run_code({ code: "return 1", description: "nested" }); } catch (e) { messages.push(e.message); }
return messages;
"#;
    let out = engine
        .invoke_tool(
            "run_code",
            json!({ "code": code, "description": "Probe error propagation paths" }),
            &context,
        )
        .await
        .expect("program executes");
    let text = out.as_str().unwrap();
    assert!(text.contains("NoSuchTool"), "{text}");
    assert!(text.contains("cannot dispatch itself"), "{text}");

    // A program that throws fails the call loudly, keeping its logs.
    let err = engine
        .invoke_tool(
            "run_code",
            json!({
                "code": "console.log(\"before the crash\");\nthrow new Error(\"deliberate\");",
                "description": "Throw after logging once",
            }),
            &context,
        )
        .await
        .expect_err("throwing program fails the call");
    let message = err.to_string();
    assert!(message.contains("deliberate"), "{message}");
    assert!(message.contains("before the crash"), "{message}");

    // The wall-clock budget is hard: a spinning program is terminated.
    let run_code = RunCodeTool::with_budget_and_runtime(
        engine.clone(),
        session_ctx.clone(),
        Duration::from_secs(1),
        code_node_runtime(),
    );
    let err = run_code
        .call(
            json!({ "code": "for (;;) {}", "description": "Spin forever" }),
            &context,
        )
        .await
        .expect_err("spinning program is terminated");
    assert!(err.to_string().contains("budget"), "{err}");

    // `timeout_ms` raises that budget for the one call that asks. The tool
    // instance is still the 1s one above, so only the argument can explain a
    // program that runs for two seconds and returns.
    let outcome = run_code
        .call(
            json!({
                "code": "const end = Date.now() + 2000; while (Date.now() < end) {} return \"outlasted\";",
                "description": "Outlast the instance budget",
                "timeout_ms": 20_000
            }),
            &context,
        )
        .await
        .expect("raised budget lets the program finish");
    assert!(
        outcome.as_str().unwrap_or_default().contains("outlasted"),
        "{outcome}"
    );

    // A budget outside the published range is rejected before anything runs.
    for bad in [json!(0), json!(600_001), json!("30000")] {
        let err = engine
            .invoke_tool(
                "run_code",
                json!({ "code": "return 1", "description": "Return one", "timeout_ms": bad }),
                &context,
            )
            .await
            .expect_err("out-of-range timeout_ms rejected");
        assert!(err.to_string().contains("timeout_ms"), "{err}");
    }

    // Empty description is rejected (dsh validation).
    let err = engine
        .invoke_tool(
            "run_code",
            json!({ "code": "return 1", "description": "  " }),
            &context,
        )
        .await
        .expect_err("empty description rejected");
    assert!(err.to_string().contains("description"), "{err}");
}
