//! The tool-invoke seat — composed plugins call rebon's **core** tools
//! through the kernel, with the real engine pipeline (schema validation,
//! tool-local validation, permission brokerage) on every call.
//!
//! Seat membership is a closed set: filesystem, search, shell, and web
//! tools. Orchestration tools (Agent/Task/Team/Workflow families) are the
//! loop's own features and deliberately NOT in the seat — invoking one
//! fails `[NOT_IN_SEAT]`, never silently.
//!
//! Authorization follows the credentialGrants precedent: writing a tool
//! name into `kernelPlugins.toolGrants` is the user's explicit consent for
//! composed plugins to run that non-read-only tool. Read-only calls run
//! without a grant (still confined to the workspace root by path scopes);
//! ungranted non-read-only calls fail closed `[PERMISSION_REQUIRED]` — the
//! interactive ask pipeline layers on top of, not replaces,
//! this grant surface.
//!
//! The host owns its own [`Engine`] instance (builtin tool catalog + invoke
//! pipeline). Session-scoped contexts (per-session cwd, session file
//! history) arrive with the session seat; the composition plane is
//! process-global, so its workspace root is fixed at bind time.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use rebon_core::Engine;
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::{ToolInvocation, ToolInvoker, ToolRefusal};
use rebon_tool::{AgentRegistry, PermissionBroker, Tool, ToolContext};
use rebon_tools_core::{
    FileStateCache, PermissionBehavior, PermissionDecision, ToolError, ToolResult,
};
use serde_json::Value;

/// Closed membership of the tool seat. Names must match the engine's
/// builtin catalog (pinned by `core_seat_names_all_resolve`).
pub const CORE_TOOL_SEAT: &[&str] = &[
    // filesystem
    "Read",
    rebon_tool::FILE_WRITE_TOOL_NAME,
    rebon_tool::FILE_EDIT_TOOL_NAME,
    rebon_tool::FILE_MULTI_EDIT_TOOL_NAME,
    rebon_tool::NOTEBOOK_EDIT_TOOL_NAME,
    // search
    "Glob",
    "Grep",
    // shell
    "Bash",
    "PowerShell",
    // web
    rebon_tool::WEB_SEARCH_TOOL_NAME,
    rebon_tool::WEB_FETCH_TOOL_NAME,
];

/// Parse `kernelPlugins.toolGrants`: tool names the user has explicitly
/// allowed composed plugins to run without an interactive ask.
pub fn load_tool_grants(config_dir: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read(config_dir.join("config.json")) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_slice::<Value>(&raw) else {
        return Vec::new();
    };
    let mut grants: Vec<String> = config
        .get("kernelPlugins")
        .and_then(|section| section.get("toolGrants"))
        .and_then(|list| list.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    grants.sort();
    grants.dedup();
    grants
}

/// Permission broker for seat invocations: Allow runs, Deny refuses, Ask
/// consults the config grants first — granted runs; ungranted suspends on
/// the [`ToolAskService`] surface when one is bound and otherwise
/// fails closed with a stable code.
struct SeatToolBroker {
    granted: HashSet<String>,
    asks: Option<Arc<crate::kernel_tool_asks::ToolAskService>>,
}

#[async_trait]
impl PermissionBroker for SeatToolBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        let effective_input = decision.updated_input.unwrap_or(input);
        match decision.behavior {
            PermissionBehavior::Allow => tool.call(effective_input, context).await,
            PermissionBehavior::Deny => Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: format!(
                    "[PERMISSION_DENIED] {}",
                    decision.reason.unwrap_or_else(|| "denied by policy".into())
                ),
            }),
            PermissionBehavior::Ask => {
                if self.granted.contains(tool.id().as_str()) {
                    return tool.call(effective_input, context).await;
                }
                let Some(asks) = &self.asks else {
                    return Err(ToolError::PermissionDenied {
                        tool: tool.id(),
                        reason: format!(
                            "[PERMISSION_REQUIRED] tool `{}` needs authorization; grant it \
                             via kernelPlugins.toolGrants (no interactive ask surface is \
                             bound to this plane)",
                            tool.id().as_str()
                        ),
                    });
                };
                let request = decision
                    .request
                    .as_ref()
                    .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null);
                use crate::kernel_tool_asks::AskOutcome;
                match asks
                    .ask(tool.id().as_str(), request, &effective_input)
                    .await
                {
                    AskOutcome::Allow => tool.call(effective_input, context).await,
                    AskOutcome::Deny => Err(ToolError::PermissionDenied {
                        tool: tool.id(),
                        reason: format!(
                            "[PERMISSION_DENIED] tool `{}` was denied on the ask surface",
                            tool.id().as_str()
                        ),
                    }),
                    AskOutcome::Timeout => Err(ToolError::PermissionDenied {
                        tool: tool.id(),
                        reason: format!(
                            "[PERMISSION_TIMEOUT] nobody answered the authorization ask for \
                             tool `{}` before the deadline",
                            tool.id().as_str()
                        ),
                    }),
                }
            }
        }
    }
}

/// The engine-backed tool invoker: owns a builtin
/// tool catalog and serves seat invocations confined to one workspace root.
pub struct EngineToolInvokeHost {
    engine: Arc<Engine>,
    workspace_root: PathBuf,
    broker: Arc<SeatToolBroker>,
    /// Shared across invokes so read-before-edit sequences (Read → Edit)
    /// work like they do inside a session.
    file_state: FileStateCache,
}

impl EngineToolInvokeHost {
    /// `upstream` is the kernel context whose `tool-registry` seat this host's
    /// engine resolves plugin-registered tools through — the process kernel's
    /// root in production, the loop's own fork for a loop host. Passed in
    /// rather than looked up: every caller already holds the kernel it means,
    /// and a host that guessed would point a loop's engine at the process seat.
    pub fn new(
        upstream: &rebon_kernel::Context,
        workspace_root: PathBuf,
        config_dir: &Path,
        granted: Vec<String>,
    ) -> Arc<Self> {
        Self::with_ask_surface(upstream, workspace_root, config_dir, granted, None)
    }

    /// Like [`Self::new`], with the interactive suspend-answer surface
    /// bound: ungranted Ask decisions park there instead of failing closed.
    pub fn with_ask_surface(
        upstream: &rebon_kernel::Context,
        workspace_root: PathBuf,
        config_dir: &Path,
        granted: Vec<String>,
        asks: Option<Arc<crate::kernel_tool_asks::ToolAskService>>,
    ) -> Arc<Self> {
        rebon_tool::set_agent_registry_selection(
            Arc::new(AgentRegistry::load_with_plugin_dirs(
                &workspace_root,
                config_dir,
                &[],
            )),
            false,
        );
        let engine = Arc::new(Engine::with_builtin_tools());
        // The feature tools this seat names (`WebSearch`, `WebFetch`, …) are
        // registered by plugins on the process seat, not by the engine. Point
        // this engine at that seat so the plugin plane resolves exactly what
        // is loaded: switch a plugin off and its tools leave this seat too.
        engine.attach_upstream_tool_context(upstream.clone());
        Arc::new(Self {
            engine,
            workspace_root,
            broker: Arc::new(SeatToolBroker {
                granted: granted.into_iter().collect(),
                asks,
            }),
            file_state: FileStateCache::new(),
        })
    }

    /// The builtin catalog engine backing this host (shared with the
    /// compose tool registry's shadow check).
    pub fn engine(&self) -> Arc<Engine> {
        self.engine.clone()
    }

    fn tool_context(&self, tool: &str) -> ToolContext {
        let root = self.workspace_root.to_string_lossy().to_string();
        let mut context = ToolContext::new()
            .with_cwd(root)
            .with_path_scope_roots([self.workspace_root.clone()])
            .with_file_state_cache(self.file_state.clone())
            .with_permission_broker(self.broker.clone());
        // A write scope is itself an authorization: inside it, write-class
        // tools auto-allow. So it is attached only for tools the config
        // has granted — the grant buys "free writes, confined to the
        // workspace root"; ungranted tools stay on the Ask path and fail
        // closed in the broker.
        if self.broker.granted.contains(tool) {
            context = context.with_write_scope_roots([self.workspace_root.clone()]);
        }
        context
    }

    async fn invoke_inner(&self, tool: &str, input: Value) -> Result<Value, String> {
        if !CORE_TOOL_SEAT.contains(&tool) {
            return Err(format!(
                "[NOT_IN_SEAT] tool `{tool}` is not part of the kernel tool seat \
                 (core set: {})",
                CORE_TOOL_SEAT.join(", ")
            ));
        }
        let context = self.tool_context(tool);
        match self.engine.invoke_tool(tool, input, &context).await {
            Ok(value) => Ok(value),
            Err(err) => Err(render_tool_error(err)),
        }
    }
}

/// Map engine errors to seat error strings with stable bracketed codes.
/// Broker refusals already carry their own code; everything else gets one.
fn render_tool_error(err: ToolError) -> String {
    match err {
        ToolError::UnknownTool { tool } => {
            format!("[UNKNOWN_TOOL] tool `{}` is not registered", tool.as_str())
        }
        ToolError::InvalidInput { tool, reason, .. } => {
            format!("[INVALID_INPUT] {}: {reason}", tool.as_str())
        }
        ToolError::PermissionDenied { reason, tool } => {
            if reason.starts_with('[') {
                reason
            } else {
                format!("[PERMISSION_DENIED] {}: {reason}", tool.as_str())
            }
        }
        ToolError::Cancelled { tool, reason } => {
            format!("[TOOL_CANCELLED] {}: {reason}", tool.as_str())
        }
        other => format!("[TOOL_FAILED] {other}"),
    }
}

impl EngineToolInvokeHost {
    /// Seat catalog: every `CORE_TOOL_SEAT` member's engine snapshot as
    /// `{name, description, inputSchema}`. Filtered from the same catalog
    /// `invoke_inner` dispatches into, so described and dispatchable stay
    /// one fact.
    ///
    /// Public because it is what rebon hands a model-facing consumer — an
    /// agent loop's prompt assembly — and that consumer should not have to go
    /// through a runtime-specific trait to ask.
    pub fn describe_catalog(&self) -> Value {
        self.describe_inner()
    }

    fn describe_inner(&self) -> Value {
        let described: Vec<Value> = self
            .engine
            .tool_snapshots()
            .into_iter()
            .filter(|snapshot| CORE_TOOL_SEAT.contains(&snapshot.name.as_str()))
            .map(|snapshot| {
                serde_json::json!({
                    "name": snapshot.name,
                    "description": snapshot.description,
                    "inputSchema": snapshot.input_schema,
                })
            })
            .collect();
        Value::Array(described)
    }
}

/// The plugin plane's tool seam, served by the same engine and the same
/// permission broker a session uses.
///
/// Two gates have already run by the time this is called — the plugin's
/// manifest declared the tool, and the plane's exposed set contains it — so
/// what is left is the question those two cannot answer: whether the *user*
/// agreed to this particular use. That is `invoke_inner`'s broker chain, which
/// is why this is a thin wrapper over it rather than a second path.
impl ToolInvoker for EngineToolInvokeHost {
    fn invoke(
        &self,
        invocation: ToolInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<Payload, ToolRefusal>> + Send + '_>> {
        let input = invocation.input.to_value().unwrap_or(Value::Null);
        Box::pin(async move {
            match self.invoke_inner(&invocation.tool, input).await {
                Ok(value) => Ok(Payload::from(value)),
                // The refusal already begins with a stable bracketed code;
                // splitting it out gives the plane its own error shape without
                // inventing a second vocabulary for the same refusals.
                Err(message) => Err(ToolRefusal::new(bracketed_code(&message), message)),
            }
        })
    }
}

/// The `[CODE]` a seat refusal begins with, or a generic one.
fn bracketed_code(message: &str) -> String {
    message
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .map(|(code, _)| format!("[{code}]"))
        .unwrap_or_else(|| "[TOOL_FAILED]".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kernel per host, which is what these tests want: nothing registers
    /// on the upstream seat, so every resolution below is the engine's own
    /// builtin catalog answering — the property each of them is about.
    fn host_with_grants(root: &Path, grants: &[&str]) -> Arc<EngineToolInvokeHost> {
        let kernel = rebon_kernel::Kernel::new();
        EngineToolInvokeHost::new(
            kernel.context(),
            root.to_path_buf(),
            &root.join(".rebon-config"),
            grants.iter().map(|g| g.to_string()).collect(),
        )
    }

    fn code_of(err: &str) -> &str {
        err.split(']')
            .next()
            .map(|c| c.trim_start_matches('['))
            .unwrap_or("")
    }

    #[tokio::test]
    async fn read_and_grep_serve_workspace_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "hello from the seat\n").unwrap();
        let host = host_with_grants(dir.path(), &[]);

        let read = host
            .invoke_inner(
                "Read",
                serde_json::json!({ "file_path": file.to_string_lossy() }),
            )
            .await
            .expect("Read succeeds without any grant");
        assert!(
            read.to_string().contains("hello from the seat"),
            "read result must carry the file content: {read}"
        );

        let grep = host
            .invoke_inner(
                "Grep",
                serde_json::json!({
                    "pattern": "hello from",
                    "path": dir.path().to_string_lossy(),
                }),
            )
            .await
            .expect("Grep succeeds without any grant");
        assert!(
            grep.to_string().contains("hello.txt"),
            "grep result must name the matching file: {grep}"
        );
    }

    #[tokio::test]
    async fn write_fails_closed_without_grant_and_runs_with_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        let input = serde_json::json!({
            "file_path": target.to_string_lossy(),
            "content": "granted write\n",
        });

        let ungranted = host_with_grants(dir.path(), &[]);
        let err = ungranted
            .invoke_inner("Write", input.clone())
            .await
            .expect_err("ungranted write must fail closed");
        assert_eq!(code_of(&err), "PERMISSION_REQUIRED", "{err}");
        assert!(!target.exists(), "nothing may land on disk");

        let granted = host_with_grants(dir.path(), &["Write"]);
        granted
            .invoke_inner("Write", input)
            .await
            .expect("granted write runs");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "granted write\n");
    }

    #[tokio::test]
    async fn orchestration_tools_are_not_in_seat() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_with_grants(dir.path(), &["Agent", "TaskCreate"]);
        // Even a (nonsensical) grant cannot smuggle an orchestration tool
        // into the seat: membership is checked first.
        for name in ["Agent", "TaskCreate", "Workflow", "SendMessage", "Skill"] {
            let err = host
                .invoke_inner(name, serde_json::json!({}))
                .await
                .expect_err("orchestration tool must be refused");
            assert_eq!(code_of(&err), "NOT_IN_SEAT", "{name}: {err}");
        }
    }

    /// An ungranted write with a bound ask surface suspends instead
    /// of failing closed; the answer resumes it — allow lands the write,
    /// deny surfaces the stable code. The suspension itself is plain
    /// async: the test answers from the same runtime while the invoke is
    /// parked, which is exactly the not-frozen property.
    #[tokio::test]
    async fn ungranted_write_suspends_on_the_ask_surface() {
        use crate::kernel_tool_asks::ToolAskService;

        let dir = tempfile::tempdir().unwrap();
        let kernel = rebon_kernel::Kernel::new();
        let asks = ToolAskService::new(
            kernel.context().fork("tool-asks"),
            std::time::Duration::from_secs(30),
        );
        let host = EngineToolInvokeHost::with_ask_surface(
            kernel.context(),
            dir.path().to_path_buf(),
            &dir.path().join(".rebon-config"),
            Vec::new(),
            Some(asks.clone()),
        );

        // Allow path: park the invoke, answer from outside, expect the file.
        let target = dir.path().join("asked.txt");
        let input = serde_json::json!({
            "file_path": target.to_string_lossy(),
            "content": "approved via ask\n",
        });
        let invoking = {
            let host = host.clone();
            let input = input.clone();
            tokio::spawn(async move { host.invoke_inner("Write", input).await })
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let id = loop {
            if let Some(first) = asks.list().as_array().and_then(|a| a.first()) {
                assert_eq!(first["tool"], "Write");
                assert_eq!(first["preview"]["content"], "approved via ask\n");
                break first["id"].as_u64().unwrap();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the suspended write never surfaced on the ask list"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        asks.answer(id, true).unwrap();
        invoking.await.unwrap().expect("approved write runs");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "approved via ask\n"
        );

        // Deny path.
        let denied_target = dir.path().join("denied.txt");
        let invoking = {
            let host = host.clone();
            let input = serde_json::json!({
                "file_path": denied_target.to_string_lossy(),
                "content": "never\n",
            });
            tokio::spawn(async move { host.invoke_inner("Write", input).await })
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let id = loop {
            if let Some(first) = asks.list().as_array().and_then(|a| a.first()) {
                break first["id"].as_u64().unwrap();
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        asks.answer(id, false).unwrap();
        let err = invoking.await.unwrap().expect_err("denied write refuses");
        assert_eq!(code_of(&err), "PERMISSION_DENIED", "{err}");
        assert!(!denied_target.exists(), "denied write must not land");
    }

    #[test]
    fn tool_grants_parse_trimmed_and_deduped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{
                "kernelPlugins": {
                    "plugins": [{ "id": "x", "name": "x" }],
                    "toolGrants": ["Write", " ", "Write", "Bash"]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(load_tool_grants(tmp.path()), vec!["Bash", "Write"]);

        std::fs::write(tmp.path().join("config.json"), r#"{}"#).unwrap();
        assert!(load_tool_grants(tmp.path()).is_empty());
    }
}
