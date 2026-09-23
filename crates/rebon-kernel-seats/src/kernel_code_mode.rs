//! PTC / Code Mode: the `run_code` tool — the
//! model writes ONE program that orchestrates a chain of tool calls
//! (parallel, conditional, loops, intermediate processing) instead of
//! round-tripping through the model per call.
//!
//! Model-facing contract transcribed from dsh Code Mode: `code` is the
//! BODY of an async function (top-level `await`/`return`), tools are
//! called `await tools.name(args)`, only what the program prints or
//! returns comes back. Output renders as dsh does: logs line by line,
//! then the result (strings raw, everything else pretty JSON), or
//! `(run_code completed with no output)`.
//!
//! Permission model: the transport itself asks nothing — the isolate has
//! NO ambient I/O (no fs/net/env ops), so a program is pure computation
//! plus gated tool calls, and every nested dispatch runs the FULL engine
//! pipeline against the session's own ToolContext (read-only tools
//! auto-allow, write-class tools Ask through the session broker exactly
//! as a direct call would). `run_code` cannot call itself.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use rebon_code_runner::{
    run_code_program_with_runtime, CodeNodeRuntime, CodeOutcome, CodeProgramError,
    CodeToolDispatcher,
};
use rebon_core::Engine;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    ToolError, ToolId, ToolInputSchema, ToolProgressUpdate, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};

/// dsh `RUN_CODE_NAME`.
pub const RUN_CODE_TOOL_NAME: &str = "run_code";

/// Wall-clock budget for one program run when `timeout_ms` is absent.
const DEFAULT_PROGRAM_BUDGET: Duration = Duration::from_secs(120);

/// Ceiling for a caller-supplied `timeout_ms`.
///
/// The budget is wall-clock and it starts at process launch, so it covers the
/// time the program spends *waiting on nested dispatch*, not just its own
/// computation. A program that fans out to slow tools — sub-agents above all —
/// can blow the 120s default through no fault of its own, which is what this
/// argument exists to raise.
const MAX_PROGRAM_BUDGET_MS: u64 = 600_000;

/// Resolve the wall-clock budget for one call.
///
/// Absent or unusable input keeps `fallback` — the instance budget, which
/// production sets to [`DEFAULT_PROGRAM_BUDGET`] and tests set short. A value
/// that made it past `validate_input` is still clamped here: `call` is
/// reachable on its own, and a budget is not a thing to take on trust from the
/// model.
fn resolve_program_budget(input: &Value, fallback: Duration) -> Duration {
    match input.get("timeout_ms").and_then(Value::as_u64) {
        Some(ms) if ms > 0 => Duration::from_millis(ms.min(MAX_PROGRAM_BUDGET_MS)),
        _ => fallback,
    }
}

/// Concurrent nested dispatch cap (dsh `maxParallelSubCalls` default).
const DEFAULT_MAX_PARALLEL: usize = 10;

/// The Node that Code Mode's one-shot processes run on.
///
/// Two rungs, and `PATH` is deliberately not one of them. The first is the
/// launcher contract — an npm-distributed rebon was itself started by a Node, so
/// it hands that one down. The second is rebon's own managed install, whose
/// bytes had to match a digest compiled into this binary before they were
/// unpacked.
///
/// The plugin plane's ladder ends in `PATH` and this one does not, for the
/// reason recorded in `rebon-node-runtime`: plugins are trusted local code, so
/// the user's own Node is exactly right for them, while Code Mode executes code
/// the model wrote and the provenance of the executable is part of its threat
/// model. What this adds is the managed rung — before it, a rebon that was not
/// launched by npm had no Code Mode at all, even with a vetted runtime already
/// installed under its own config home.
///
/// Resolved once per process. Both rungs run the runtime to check it can be
/// hardened, and that answer does not change while rebon is running.
pub fn code_mode_runtime() -> Result<&'static CodeNodeRuntime, &'static CodeProgramError> {
    static RUNTIME: std::sync::OnceLock<Result<CodeNodeRuntime, CodeProgramError>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let launcher = match CodeNodeRuntime::discover() {
                Ok(runtime) => return Ok(runtime),
                Err(error) => error,
            };
            let managed = rebon_node_runtime::ManagedRuntimeStore::under_config_home(
                &rebon_config::config_home_dir(),
            );
            // Newest first: a store may hold more than one install, and the
            // newest vetted runtime is the one whose flag set is most likely to
            // still be the supported spelling.
            let mut installed = managed.installed();
            installed.sort_by(|left, right| right.version.cmp(&left.version));
            let mut refusals = Vec::new();
            for runtime in installed {
                match CodeNodeRuntime::new(runtime.executable) {
                    Ok(runtime) => return Ok(runtime),
                    Err(error) => refusals.push(error.message),
                }
            }
            let mut message = launcher.message;
            if refusals.is_empty() {
                message.push_str(
                    "; no managed Node runtime is installed either — `rebon node install` \
                     provides one",
                );
            } else {
                message.push_str("; the managed runtimes were refused too: ");
                message.push_str(&refusals.join("; "));
            }
            Err(CodeProgramError {
                message,
                logs: Vec::new(),
            })
        })
        .as_ref()
}

/// Whether a Node that could run a program is reachable at all.
///
/// Cheap on purpose: two `stat`s for the launcher's runtime, a directory
/// listing for the managed store, and the answer is cached. It is read from
/// tool exposure, which runs for every tool on every snapshot, so it cannot
/// afford [`code_mode_runtime`]'s spawn — and cannot wait for it either,
/// because that probe is what makes the *first* `run_code` call slow, not
/// the tool list.
///
/// This is the difference between "the runtime refused to be hardened" and
/// "there is no runtime". The first is worth telling the model about when it
/// calls; the second means the tool should never have been offered. Terminal-Bench 4.0
/// r1 offered it twice into a container with no Node at all, and got the
/// answer it had to get both times.
pub fn code_mode_runtime_reachable() -> bool {
    static REACHABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REACHABLE.get_or_init(|| {
        if rebon_code_runner::locate_executable().is_some() {
            return true;
        }
        !rebon_node_runtime::ManagedRuntimeStore::under_config_home(
            &rebon_config::config_home_dir(),
        )
        .installed()
        .is_empty()
    })
}

pub const PLUGIN_ID: &str = "code-mode";
const DEFAULT_ON_SETTING: &str = "defaultOn";

pub fn default_on_in(config_dir: &std::path::Path, cwd: &std::path::Path) -> bool {
    rebon_config::plugin_settings_in(config_dir, cwd, PLUGIN_ID)
        .get(DEFAULT_ON_SETTING)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

struct ExperimentService;
impl rebon_kernel::Service for ExperimentService {
    type Interface = ();
    const NAME: &'static str = "code-mode-experiment";
}

pub struct CodeModePlugin;
impl rebon_kernel::Plugin for CodeModePlugin {
    fn meta(&self) -> rebon_kernel::PluginMeta {
        rebon_kernel::PluginMeta::new(PLUGIN_ID).provides(&["code-mode-experiment"])
    }

    fn apply(&self, ctx: &rebon_kernel::Context) -> Result<(), rebon_kernel::KernelError> {
        ctx.provide::<ExperimentService>(Arc::new(()))
    }
}

pub static PLUGIN: rebon_kernel::PluginDef = rebon_kernel::PluginDef {
    id: PLUGIN_ID,
    title: "实验性 Code Mode（默认关闭）",
    kind: rebon_kernel::PluginKind::Feature,
    default_enabled: false,
    factory: |_| Ok(Box::new(CodeModePlugin)),
};

pub struct CodeModeSessionService;
impl rebon_kernel::Service for CodeModeSessionService {
    type Interface = RunCodeTool;
    const NAME: &'static str = "code-mode-session";
}

pub fn command(ctx: &rebon_kernel::Context, args: &[String]) -> Result<String, String> {
    ctx.get::<CodeModeSessionService>()
        .ok_or_else(|| "当前会话未绑定 Code Mode 服务".to_string())?
        .command(args)
}

/// The `run_code` tool for one session.
pub struct RunCodeTool {
    engine: Arc<Engine>,
    /// Session kernel context: nested-dispatch lifecycle events
    /// (`code/dispatch-start` / `code/dispatch`) emit here for UI faces.
    events: rebon_kernel::Context,
    budget: Duration,
    max_parallel: usize,
    node_runtime: Option<CodeNodeRuntime>,
    // 启用状态只对当前实验插件实例有效；卸载再开放不会自动恢复开启。
    activation: Mutex<Option<Weak<()>>>,
}

impl RunCodeTool {
    fn requested(&self) -> bool {
        let current = self.events.get::<ExperimentService>();
        let activated = self
            .activation
            .lock()
            .expect("Code Mode 状态锁损坏")
            .as_ref()
            .and_then(Weak::upgrade);
        matches!((current, activated), (Some(current), Some(activated)) if Arc::ptr_eq(&current, &activated))
    }

    pub fn command(&self, args: &[String]) -> Result<String, String> {
        match args {
            [] => Ok(format!(
                "Code Mode: {}（当前会话）",
                if self.requested() { "on" } else { "off" }
            )),
            [arg] if arg == "off" => {
                *self.activation.lock().expect("Code Mode 状态锁损坏") = None;
                Ok("Code Mode: off（当前会话）".into())
            }
            [arg] if arg == "on" => {
                let experiment = self.events.get::<ExperimentService>().ok_or_else(|| {
                    "Code Mode 实验特性尚未开放。请先执行 /kernel enable code-mode，再执行 /codemode on。也可在 ~/.rebon/settings.json（或项目 .rebon/settings.json）中设置 {\"plugins\":{\"code-mode\":{\"enabled\":true}}}，重启后再执行 /codemode on；开放实验本身不会开启当前会话。".to_string()
                })?;
                *self.activation.lock().expect("Code Mode 状态锁损坏") =
                    Some(Arc::downgrade(&experiment));
                Ok(if self.is_enabled() {
                    "Code Mode: on（仅当前会话；run_code 优先，权限与工具过滤仍生效）".into()
                } else {
                    "Code Mode: on（仅当前会话）；run_code 暂不可用：缺少受信任的 Node runtime。请执行 rebon node install 后重启并重新 /codemode on。".into()
                })
            }
            _ => Err("用法：/codemode [on|off]".into()),
        }
    }

    pub fn new(engine: Arc<Engine>, events: rebon_kernel::Context, default_on: bool) -> Arc<Self> {
        let activation = if default_on {
            events
                .get::<ExperimentService>()
                .map(|experiment| Arc::downgrade(&experiment))
        } else {
            None
        };
        Arc::new(Self {
            engine,
            events,
            budget: DEFAULT_PROGRAM_BUDGET,
            max_parallel: DEFAULT_MAX_PARALLEL,
            node_runtime: None,
            activation: Mutex::new(activation),
        })
    }

    /// Test hook: a short budget makes the timeout path assertable.
    pub fn with_budget(
        engine: Arc<Engine>,
        events: rebon_kernel::Context,
        budget: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            events,
            budget,
            max_parallel: DEFAULT_MAX_PARALLEL,
            node_runtime: None,
            activation: Mutex::new(None),
        })
    }

    /// Test/embedding hook for an explicit, absolute Node runtime. Production uses the packaged
    /// launcher contract resolved by [`CodeNodeRuntime::discover`].
    pub fn with_budget_and_runtime(
        engine: Arc<Engine>,
        events: rebon_kernel::Context,
        budget: Duration,
        node_runtime: CodeNodeRuntime,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            events,
            budget,
            max_parallel: DEFAULT_MAX_PARALLEL,
            node_runtime: Some(node_runtime),
            activation: Mutex::new(None),
        })
    }
}

/// dsh's render contract for the `{logs, result}` outcome.
fn render_outcome(outcome: &CodeOutcome) -> String {
    let rendered = match &outcome.result {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(value) => {
            serde_json::to_string_pretty(value).expect("serde_json::Value always serializes")
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if !outcome.logs.is_empty() {
        parts.push(outcome.logs.join("\n"));
    }
    if !rendered.is_empty() {
        parts.push(rendered);
    }
    if parts.is_empty() {
        "(run_code completed with no output)".to_string()
    } else {
        parts.join("\n")
    }
}

/// Progress kinds this tool emits. A surface that wants to render Code Mode
/// specially keys off these; one that does not shows the messages as text,
/// which is why each message is written to read on its own.
pub const CODE_PROGRAM_KIND: &str = "code_mode/program";
pub const CODE_DISPATCH_START_KIND: &str = "code_mode/dispatch-start";
pub const CODE_DISPATCH_END_KIND: &str = "code_mode/dispatch";

/// How much of a nested call's arguments a progress line carries.
const DISPATCH_ARGS_MAX: usize = 120;

/// The source is already in the call input; progress must not repeat it into
/// the default transcript body, where a single minified line can fill a screen.
fn program_message(code: &str) -> String {
    let lines = code.trim_end().lines().count();
    let unit = if lines == 1 { "line" } else { "lines" };
    format!("Running JavaScript ({lines} {unit})")
}

/// One nested call, on its way out.
fn dispatch_start_message(tool: &str, input: &Value) -> String {
    let rendered = input
        .get("description")
        .and_then(Value::as_str)
        .filter(|description| !description.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            rebon_tools_core::primary_display_params(tool).map(|keys| {
                keys.iter()
                    .filter_map(|key| input.get(*key))
                    .filter(|value| !value.is_null())
                    .map(|value| match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        })
        .unwrap_or_else(|| match input {
            Value::Null => String::new(),
            Value::Object(map) if map.is_empty() => String::new(),
            other => other.to_string(),
        });
    let rendered = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    if rendered.is_empty() {
        return format!("→ {tool}");
    }
    let args = if rendered.chars().count() > DISPATCH_ARGS_MAX {
        let head: String = rendered.chars().take(DISPATCH_ARGS_MAX).collect();
        format!("{head}…")
    } else {
        rendered
    };
    format!("→ {tool} ({args})")
}

/// The same call, coming back. Failure keeps its message: a program that ran
/// ten tools and got one refusal is a different thing to read than one that
/// simply took a while.
fn dispatch_end_message(tool: &str, outcome: Result<(), &str>) -> String {
    match outcome {
        Ok(()) => format!("← {tool} succeeded"),
        Err(error) => {
            let first = error.lines().next().unwrap_or(error);
            format!("← {tool} failed: {first}")
        }
    }
}

struct EngineCodeDispatcher {
    engine: Arc<Engine>,
    context: ToolContext,
    events: rebon_kernel::Context,
    semaphore: Arc<tokio::sync::Semaphore>,
    sequence: std::sync::atomic::AtomicU64,
}

impl CodeToolDispatcher for EngineCodeDispatcher {
    fn dispatch(&self, name: String, input: Value) -> BoxFuture<'static, Result<Value, String>> {
        let engine = self.engine.clone();
        let context = self.context.clone();
        let events = self.events.clone();
        let semaphore = self.semaphore.clone();
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(async move {
            if name == RUN_CODE_TOOL_NAME {
                return Err(format!(
                    "[NOT_IN_SEAT] {RUN_CODE_TOOL_NAME} cannot dispatch itself — orchestrate within one program"
                ));
            }
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|_| "code dispatch pool closed".to_string())?;
            events.emit_json(
                "code/dispatch-start",
                &json!({ "seq": sequence, "tool": name }),
            );
            // The kernel event above is for components inside this process; the
            // progress below is what reaches a person. Both, because they have
            // different audiences and only one of them survives the process
            // boundary a session update crosses.
            context.emit_progress(
                ToolProgressUpdate::new(CODE_DISPATCH_START_KIND)
                    .with_message(dispatch_start_message(&name, &input))
                    .with_payload(json!({ "seq": sequence, "tool": name })),
            );
            let outcome = engine.invoke_tool(&name, input, &context).await;
            let is_error = outcome.is_err();
            events.emit_json(
                "code/dispatch",
                &json!({ "seq": sequence, "tool": name, "isError": is_error }),
            );
            let failure = outcome.as_ref().err().map(ToString::to_string);
            context.emit_progress(
                ToolProgressUpdate::new(CODE_DISPATCH_END_KIND)
                    .with_message(dispatch_end_message(
                        &name,
                        failure.as_deref().map_or(Ok(()), Err),
                    ))
                    .with_payload(json!({ "seq": sequence, "tool": name, "isError": is_error })),
            );
            outcome.map_err(|err| err.to_string())
        })
    }
}

// dsh's TypeScript flavor, restated for the embedded JavaScript
// runtime (same contract: async-function body, tools proxy,
// curate what comes back).
const RUN_CODE_DESCRIPTION: &str =
    "Execute a JavaScript program against the available tools. Takes two required \
    arguments: `code`, the BODY of an async function (top-level `await` and `return` \
    work), and `description`, a short summary of what the program does. Call tools as \
    `await tools.name(args)` — the same tools and schemas you already have, with the \
    same permissions. Use it to orchestrate a CHAIN of tool calls in one step: loops, \
    conditionals, parallel calls, and intermediate processing. Only what you print \
    (console.log) or return comes back — curate it.\n\n\
    Arguments inside `code` are JavaScript, not direct tool JSON. For regex patterns, \
    prefer String.raw`spawn\\(` so JavaScript preserves the backslash; the outer JSON \
    escaping still applies. Prefer forward slashes in Windows paths, e.g. C:/repo. \
    For Glob, use a known-existing root as `path` and discover files with `pattern` \
    (e.g. **/tests/**/*.rs), rather than guessing directories.\n\n\
    For independent exploratory reads/searches where partial results are useful, use \
    `Promise.allSettled`. Print fulfilled results and each rejected query's identity \
    and error explicitly (query identity and error.message); serializing an Error \
    object alone loses its message. Do not silently discard failures or report partial \
    results as complete. Dependent operations must run sequentially with `await` and \
    stop on failure; do not continue after a failed prerequisite. Use `Promise.all` \
    for independent calls only when any rejection should fail the program.\n\n\
    The whole program shares one wall-clock budget — 120000ms by default — and the \
    wait on every nested tool call is spent from it. When the program dispatches \
    something slow, sub-agents above all, raise `timeout_ms` (maximum 600000) to \
    cover the calls it makes rather than letting the program be killed mid-run.";

/// `run_code`'s input schema, free of the tool instance so it is assertable.
fn run_code_input_schema() -> ToolInputSchema {
    json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "The program: the body of an async JavaScript function."
            },
            "description": {
                "type": "string",
                "description": "Clear, concise description of what this program does in \
                    active voice, 5-10 words (shown in the UI). Examples: \"Count TODO \
                    markers across packages\"; \"Read failing test and its fixture\"."
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_PROGRAM_BUDGET_MS,
                "description": "Optional wall-clock budget for the whole program, in \
                    milliseconds. Default 120000, maximum 600000. The budget covers \
                    the time nested tool calls take, so raise it when the program \
                    dispatches slow tools (sub-agents, long searches); the program \
                    is killed when it runs out."
            }
        },
        "required": ["code", "description"],
        "additionalProperties": false
    })
}

#[async_trait]
impl Tool for RunCodeTool {
    fn id(&self) -> ToolId {
        ToolId::new(RUN_CODE_TOOL_NAME)
    }

    fn description(&self) -> &str {
        RUN_CODE_DESCRIPTION
    }

    fn input_schema(&self) -> ToolInputSchema {
        run_code_input_schema()
    }

    fn is_enabled(&self) -> bool {
        self.requested() && (self.node_runtime.is_some() || code_mode_runtime_reachable())
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    /// The transport asks nothing itself: the program's only doorway is
    /// nested dispatch, and every nested call runs its own full
    /// permission pipeline. See the module docs.
    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if code.trim().is_empty() {
            return Ok(ValidationOutcome::invalid("code must not be empty", 400));
        }
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if description.trim().is_empty() {
            return Ok(ValidationOutcome::invalid(
                "invalid description: expected a non-empty string",
                400,
            ));
        }
        match input.get("timeout_ms") {
            None | Some(Value::Null) => {}
            Some(value) => match value.as_u64() {
                Some(ms) if ms > 0 && ms <= MAX_PROGRAM_BUDGET_MS => {}
                _ => {
                    return Ok(ValidationOutcome::invalid(
                        format!(
                            "timeout_ms must be an integer between 1 and {MAX_PROGRAM_BUDGET_MS}"
                        ),
                        400,
                    ))
                }
            },
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        if !self.requested() {
            return Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("Code Mode 未开启；需要开放实验特性并执行 /codemode on"),
            });
        }
        let code = input
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: self.id(),
                reason: "code must be a string".into(),
                error_code: Some(400),
            })?;
        let dispatcher: Arc<dyn CodeToolDispatcher> = Arc::new(EngineCodeDispatcher {
            engine: self.engine.clone(),
            context: context.clone(),
            events: self.events.clone(),
            semaphore: Arc::new(tokio::sync::Semaphore::new(self.max_parallel)),
            sequence: std::sync::atomic::AtomicU64::new(0),
        });
        let runtime = match &self.node_runtime {
            Some(runtime) => runtime.clone(),
            None => {
                // Resolving the runtime *runs* it — twice, if the first
                // permission-flag spelling is refused — and reads the managed
                // store off disk. That happens once per process, but the once is
                // some model's first `run_code`, and a runtime worker should not
                // be the thread that sits through a Node startup for it. The
                // `OnceLock` inside still admits exactly one initialiser; what
                // changes is which pool the callers that lose the race block on.
                let resolved = tokio::task::spawn_blocking(|| {
                    code_mode_runtime().map(Clone::clone).map_err(Clone::clone)
                })
                .await
                .map_err(|error| ToolError::Execution {
                    tool: self.id(),
                    source: anyhow::anyhow!("Code Mode runtime lookup did not complete: {error}"),
                })?;
                match resolved {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        return Err(ToolError::Execution {
                            tool: self.id(),
                            source: anyhow::anyhow!("{}", error.message),
                        })
                    }
                }
            }
        };
        // Pure computation may never dispatch a tool, so it still needs a
        // visible start signal without echoing its entire source.
        context.emit_progress(
            ToolProgressUpdate::new(CODE_PROGRAM_KIND)
                .with_message(program_message(&code))
                .with_payload(json!({ "language": "javascript" })),
        );
        let budget = resolve_program_budget(&input, self.budget);
        let execution = run_code_program_with_runtime(code, dispatcher, budget, runtime).await;
        match execution {
            Ok(outcome) => Ok(Value::String(render_outcome(&outcome))),
            Err(err) => {
                // The thrown error plus whatever the program logged before
                // failing — logs are real observations, never discarded.
                let mut message = format!("program failed: {}", err.message);
                if !err.logs.is_empty() {
                    message.push_str("\nlogs before failure:\n");
                    message.push_str(&err.logs.join("\n"));
                }
                Err(ToolError::Execution {
                    tool: self.id(),
                    source: anyhow::anyhow!("{message}"),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK: Duration = Duration::from_secs(120);

    #[test]
    fn budget_falls_back_when_timeout_is_absent_or_unusable() {
        for input in [
            json!({ "code": "return 1" }),
            json!({ "code": "return 1", "timeout_ms": Value::Null }),
            json!({ "code": "return 1", "timeout_ms": 0 }),
            json!({ "code": "return 1", "timeout_ms": "30000" }),
            json!({ "code": "return 1", "timeout_ms": -5 }),
        ] {
            assert_eq!(
                resolve_program_budget(&input, FALLBACK),
                FALLBACK,
                "{input} should have kept the instance budget"
            );
        }
    }

    #[test]
    fn budget_takes_the_requested_timeout() {
        let input = json!({ "code": "return 1", "timeout_ms": 300_000 });
        assert_eq!(
            resolve_program_budget(&input, FALLBACK),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn budget_clamps_a_request_above_the_ceiling() {
        let input = json!({ "code": "return 1", "timeout_ms": 5_000_000 });
        assert_eq!(
            resolve_program_budget(&input, FALLBACK),
            Duration::from_millis(MAX_PROGRAM_BUDGET_MS)
        );
    }

    #[test]
    fn schema_publishes_the_optional_timeout() {
        let schema = run_code_input_schema();
        let timeout = &schema["properties"]["timeout_ms"];
        assert_eq!(timeout["type"], json!("integer"));
        assert_eq!(timeout["maximum"], json!(600_000));
        assert_eq!(timeout["minimum"], json!(1));
        assert_eq!(schema["required"], json!(["code", "description"]));
        assert!(RUN_CODE_DESCRIPTION.contains("`timeout_ms` (maximum 600000)"));
    }

    #[test]
    fn description_preserves_regex_escaping_guidance() {
        assert!(RUN_CODE_DESCRIPTION.contains(r"String.raw`spawn\(`"));
        assert!(RUN_CODE_DESCRIPTION.contains("JSON escaping"));
    }

    #[test]
    fn description_uses_known_search_roots() {
        assert!(RUN_CODE_DESCRIPTION.contains("forward slashes"));
        assert!(RUN_CODE_DESCRIPTION.contains("known-existing root"));
        assert!(RUN_CODE_DESCRIPTION.contains("rather than guessing directories"));
    }

    #[test]
    fn description_keeps_independent_query_failures_visible() {
        assert!(RUN_CODE_DESCRIPTION.contains("Promise.allSettled"));
        assert!(RUN_CODE_DESCRIPTION.contains("independent exploratory reads/searches"));
        assert!(RUN_CODE_DESCRIPTION.contains("query identity and error.message"));
        assert!(RUN_CODE_DESCRIPTION.contains("Do not silently discard failures"));
    }

    #[test]
    fn description_preserves_dependent_operation_failure() {
        assert!(RUN_CODE_DESCRIPTION.contains("Dependent operations must run sequentially"));
        assert!(RUN_CODE_DESCRIPTION.contains("stop on failure"));
    }

    /// The model-prompt plugin gates one instruction on this tool being
    /// offered, and cannot depend on this crate to spell its name; the two
    /// spellings must stay one.
    #[test]
    fn the_model_prompt_plugin_spells_run_code_the_way_code_mode_does() {
        assert_eq!(rebon_plugin_model_prompt::RUN_CODE_TOOL, RUN_CODE_TOOL_NAME);
    }

    /// What a person sees while a program runs.
    ///
    /// These strings are the whole of Code Mode's visible execution: the header
    /// carries a description, the result carries logs, and everything in
    /// between — the program summary, and each tool it reached for — arrives as
    /// progress. A surface that renders them as plain text still shows a
    /// legible account, which is why the shape is asserted rather than left to
    /// whatever `Debug` happens to print.
    #[test]
    fn progress_messages_read_as_an_account_of_the_run() {
        assert_eq!(
            program_message(
                "return 1 + 1;

"
            ),
            "Running JavaScript (1 line)",
            "trailing blank lines are not part of the program"
        );

        assert_eq!(
            dispatch_start_message("Read", &json!({ "file_path": "a.txt" })),
            "→ Read (a.txt)"
        );
        // No arguments is a real shape, and printing `{}` for it is noise.
        assert_eq!(dispatch_start_message("Now", &json!({})), "→ Now");
        assert_eq!(dispatch_start_message("Now", &Value::Null), "→ Now");

        // Long arguments are cut rather than allowed to fill the transcript.
        let long = json!({ "pattern": "x".repeat(400) });
        let cut = dispatch_start_message("Grep", &long);
        assert!(cut.ends_with("…)"), "{cut}");
        assert!(cut.chars().count() < 200, "{cut}");

        for tool in ["Read", "Edit", "TaskList"] {
            assert_eq!(
                dispatch_end_message(tool, Ok(())),
                format!("← {tool} succeeded")
            );
        }
        // A refusal is the part worth reading, and only its first line: the
        // rest is a stack the person did not ask for.
        assert_eq!(
            dispatch_end_message(
                "Write",
                Err("[DENIED] not allowed
  at line 3")
            ),
            "← Write failed: [DENIED] not allowed"
        );
    }

    #[test]
    fn code_progress_summarizes_programs_in_one_line() {
        for code in ["first();\nsecond();\n\n", "first();\r\nsecond();\r\n"] {
            assert_eq!(program_message(code), "Running JavaScript (2 lines)");
        }
        let code = format!("const values = [{}];", "1,".repeat(2000));
        assert_eq!(program_message(&code), "Running JavaScript (1 line)");
    }

    #[test]
    fn code_dispatch_progress_prefers_descriptions_and_primary_parameters() {
        let cases = [
            (
                "PowerShell",
                json!({ "command": "long script", "description": "Run runtime tests", "timeout": 600000 }),
                "→ PowerShell (Run runtime tests)",
            ),
            (
                "Bash",
                json!({ "command": "git status\n&& git diff", "description": "  " }),
                "→ Bash (git status && git diff)",
            ),
            (
                "Read",
                json!({ "file_path": "src/main.rs", "offset": 10, "limit": 20 }),
                "→ Read (src/main.rs)",
            ),
            (
                "Grep",
                json!({ "pattern": "TODO", "path": "src", "head_limit": 20 }),
                "→ Grep (TODO, src)",
            ),
            ("TaskList", json!({}), "→ TaskList"),
            ("Read", json!({ "file_path": null }), "→ Read"),
            (
                "Custom",
                json!({ "query": "status" }),
                "→ Custom ({\"query\":\"status\"})",
            ),
            (
                "Custom",
                json!({ "description": "Inspect external service", "payload": "large" }),
                "→ Custom (Inspect external service)",
            ),
        ];
        for (tool, input, expected) in cases {
            assert_eq!(dispatch_start_message(tool, &input), expected);
        }
    }

    #[test]
    fn code_dispatch_progress_bounds_unicode_without_hiding_failure() {
        let line = dispatch_start_message("Bash", &json!({ "command": "检查\n".repeat(200) }));
        assert!(line.chars().count() < 150, "{line}");
        assert!(!line.contains('\n'), "{line}");
        assert!(line.ends_with("…)"), "{line}");
        assert_eq!(
            dispatch_end_message("Bash", Err("permission denied\nstack")),
            "← Bash failed: permission denied"
        );
    }

    #[test]
    fn outcome_rendering_follows_the_dsh_contract() {
        let both = CodeOutcome {
            result: Some(json!({ "count": 3 })),
            logs: vec!["scanning".into(), "done".into()],
        };
        let text = render_outcome(&both);
        assert!(text.starts_with("scanning\ndone\n"), "{text}");
        assert!(text.contains("\"count\": 3"), "{text}");

        let string_result = CodeOutcome {
            result: Some(Value::String("plain answer".into())),
            logs: vec![],
        };
        assert_eq!(render_outcome(&string_result), "plain answer");

        let silent = CodeOutcome {
            result: None,
            logs: vec![],
        };
        assert_eq!(
            render_outcome(&silent),
            "(run_code completed with no output)"
        );
    }

    #[test]
    fn code_mode_default_setting_follows_user_project_local_layers() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let project = dir.path().join("project");
        let project_settings = project.join(".rebon");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&project_settings).unwrap();
        assert!(!default_on_in(&config_dir, &project));

        std::fs::write(
            config_dir.join("settings.json"),
            r#"{"plugins":{"code-mode":{"enabled":true,"defaultOn":true}}}"#,
        )
        .unwrap();
        assert!(default_on_in(&config_dir, &project));
        std::fs::write(
            project_settings.join("settings.json"),
            r#"{"plugins":{"code-mode":{"defaultOn":false}}}"#,
        )
        .unwrap();
        assert!(!default_on_in(&config_dir, &project));
        std::fs::write(
            project_settings.join("settings.local.json"),
            r#"{"plugins":{"code-mode":{"defaultOn":true}}}"#,
        )
        .unwrap();
        assert!(default_on_in(&config_dir, &project));
    }

    #[test]
    fn code_mode_default_setting_requires_a_boolean() {
        let dir = tempfile::tempdir().unwrap();
        for value in [r#""true""#, "null", "0"] {
            std::fs::write(
                dir.path().join("settings.json"),
                format!(r#"{{"plugins":{{"code-mode":{{"defaultOn":{value}}}}}}}"#),
            )
            .unwrap();
            assert!(!default_on_in(dir.path(), dir.path()));
        }
    }

    #[test]
    fn code_mode_default_activation_respects_experiment_and_session_overrides() {
        use rebon_kernel::Plugin;
        for experiment_open in [false, true] {
            let kernel = rebon_kernel::Kernel::new();
            let experiment = kernel.context().fork("experiment");
            if experiment_open {
                CodeModePlugin.apply(&experiment).unwrap();
            }
            let engine = Arc::new(Engine::new());
            let first =
                RunCodeTool::new(engine.clone(), kernel.context().fork_scoped("first"), true);
            let second = RunCodeTool::new(engine, kernel.context().fork_scoped("second"), false);
            assert_eq!(first.requested(), experiment_open);
            assert!(!second.requested());
            first.command(&["off".into()]).unwrap();
            assert!(!first.requested());
            if experiment_open {
                first.command(&["on".into()]).unwrap();
                experiment.dispose();
                assert!(!first.requested());
                let reopened = kernel.context().fork("reopened");
                CodeModePlugin.apply(&reopened).unwrap();
                assert!(!first.requested());
            }
        }
    }

    #[test]
    fn code_mode_experiment_and_session_gate_matrix() {
        use rebon_kernel::Plugin;
        assert!(!PLUGIN.default_enabled);
        assert_eq!(PLUGIN.kind, rebon_kernel::PluginKind::Feature);
        for experiment_open in [false, true] {
            let kernel = rebon_kernel::Kernel::new();
            let experiment = kernel.context().fork("experiment");
            if experiment_open {
                CodeModePlugin.apply(&experiment).unwrap();
            }
            let engine = Arc::new(Engine::new());
            let first =
                RunCodeTool::new(engine.clone(), kernel.context().fork_scoped("first"), false);
            let second = RunCodeTool::new(engine, kernel.context().fork_scoped("second"), false);
            assert!(!first.requested());
            assert!(!first.is_enabled());
            assert!(first.command(&[]).unwrap().contains("off"));
            for args in [vec!["invalid".into()], vec!["on".into(), "off".into()]] {
                assert!(first.command(&args).is_err());
                assert!(!first.requested());
            }
            let result = first.command(&["on".into()]);
            assert_eq!(result.is_ok(), experiment_open);
            assert_eq!(first.requested(), experiment_open);
            if !experiment_open {
                let error = result.unwrap_err();
                assert!(error.contains("\"code-mode\":{\"enabled\":true}"));
                assert!(error.contains("/codemode on"));
            }
            assert!(!second.requested());
            first.command(&["off".into()]).unwrap();
            assert!(!first.requested());
            if experiment_open {
                first.command(&["on".into()]).unwrap();
                experiment.dispose();
                assert!(!first.requested());
                assert!(!first.is_enabled());
                let reopened = kernel.context().fork("reopened");
                CodeModePlugin.apply(&reopened).unwrap();
                assert!(!first.requested());
                first.command(&["on".into()]).unwrap();
                assert!(first.requested());
            }
        }
    }
}
