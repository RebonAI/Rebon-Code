//! The process-level registration face
//! for composition-hosted dsh tools.
//!
//! The composition's `ctx.tools` seat mirrors every registration into this
//! service. It is provided on the compose fork under the SAME seat name as
//! the session service — `tool-registry` — which lands it in the ROOT
//! namespace (plain fork), while sessions' `fork_scoped` instances shadow it
//! for session consumers: exactly dsh's global-layer / scoped-layer split.
//! Register here carries no `module` — the execution body lives in the
//! compose isolate and dispatch goes over the [`ToolServePlane`].
//!
//! Sessions see these tools through [`SessionPluginTools`]'s merge (own
//! entries first), so model visibility and dispatch ride the existing shadowing
//! seam untouched: builtin > MCP > session plugin > composition plugin.
//!
//! Registration/unregistration from JS crosses `op_rebon_call_service`, so
//! the bridge effect ledger covers leaks (kill path included) and the
//! `token` in the register result guards stale sweeps — both for free.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use rebon_kernel::{JsonService, KernelError};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
};
use serde_json::Value;

/// Wall-clock budget for one composition tool call.
const COMPOSE_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Where a composition tool's body actually runs.
///
/// The registry decides what a tool *is* to rebon — its name, its schema, the
/// permission stance a session sees, the shadowing rule — and none of that
/// depends on where the body lives. Two places do: the embedded isolate's serve
/// plane, and the plugin plane's `tool/call`. Keeping the difference behind one
/// method is what lets the runtime move without the tool surface moving with
/// it.
///
/// The envelope is `{ok}` or `{err, code?}`. The plane answers with a terminal
/// instead, and its adapter puts it in this shape rather than the registry
/// learning a second one.
#[async_trait]
pub trait ComposeToolDispatch: Send + Sync {
    async fn serve(&self, tool: &str, input: Value, timeout: Duration) -> Result<Value, String>;

    /// Whether the runtime behind this dispatch is gone. A tool whose runtime
    /// has exited fails loudly rather than hanging on a call nobody will answer.
    fn has_exited(&self) -> bool;
}

#[derive(Clone)]
struct ComposeToolEntry {
    description: String,
    input_schema: Value,
    read_only: bool,
}

/// Process-level table of composition-hosted tools plus the serve plane
/// they execute on. One per composition boot; sessions reach it through
/// [`process_compose_tools`].
pub struct ComposeToolRegistry {
    entries: RwLock<HashMap<String, ComposeToolEntry>>,
    /// Builtin catalog snapshot for the shadow warning. Dispatch-side
    /// builtin-wins is structural (engine fallback order) and does not
    /// depend on this list being complete.
    builtin_names: HashSet<String>,
    /// Bound after the composition's runtime is up. It does not keep that
    /// runtime alive; a dead one fails calls loudly.
    plane: RwLock<Option<Arc<dyn ComposeToolDispatch>>>,
}

impl ComposeToolRegistry {
    pub fn new(builtin_names: impl IntoIterator<Item = String>) -> Arc<Self> {
        Arc::new(Self {
            entries: RwLock::new(HashMap::new()),
            builtin_names: builtin_names.into_iter().collect(),
            plane: RwLock::new(None),
        })
    }

    /// Hand the registry the dispatch its tools run on, once the composition's
    /// runtime is up.
    pub fn bind_plane(&self, plane: Arc<dyn ComposeToolDispatch>) {
        *self.plane.write().expect("compose tool plane poisoned") = Some(plane);
    }

    /// Number of registered composition tools (diagnostics/conformance).
    pub fn tool_count(&self) -> usize {
        self.entries
            .read()
            .expect("compose tool table poisoned")
            .len()
    }

    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries
            .read()
            .expect("compose tool table poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Build the proxy tool for `name`, if the composition registered it.
    pub fn tool(self: &Arc<Self>, name: &str) -> Option<Arc<dyn Tool>> {
        let entry = self
            .entries
            .read()
            .expect("compose tool table poisoned")
            .get(name)
            .cloned()?;
        Some(Arc::new(ComposeDshTool {
            name: name.to_string(),
            entry,
            registry: Arc::downgrade(self),
        }))
    }

    /// Read-plane rows for the session `tool-registry` list merge.
    pub fn list_rows(&self) -> Vec<Value> {
        let entries = self.entries.read().expect("compose tool table poisoned");
        let mut rows: Vec<Value> = entries
            .iter()
            .map(|(name, entry)| {
                serde_json::json!({
                    "name": name,
                    "description": entry.description,
                    "inputSchema": entry.input_schema,
                    "readOnly": entry.read_only,
                    "origin": "composition",
                })
            })
            .collect();
        rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        rows
    }

    /// Record what one entry reported, and hand back the tool rebon will
    /// offer for it.
    ///
    /// This used to be a JSON method — a plugin called `tool-registry
    /// register` with a name and a schema and got a token back, and rebon
    /// swept stale registrations by comparing tokens. None of that was ever
    /// about JavaScript: both ends were Rust, the caller was the plane, and
    /// the token existed because a JSON table has no way to say "this
    /// registration belongs to that scope". A [`Disposer`](rebon_kernel::Disposer)
    /// says exactly that, so the registration is an effect of the entry's
    /// fork now and the token has nothing left to guard.
    ///
    /// `None` when the name is empty — the one thing the JSON method
    /// refused, kept.
    pub fn declare(
        self: &Arc<Self>,
        name: &str,
        description: String,
        input_schema: Option<Value>,
        read_only: bool,
    ) -> Option<Arc<dyn Tool>> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        if self.builtin_names.contains(name) {
            tracing::warn!(
                tool = name,
                "composition tool shares a builtin tool's name; the builtin always wins at dispatch"
            );
        }
        let entry = ComposeToolEntry {
            description,
            input_schema: input_schema.unwrap_or_else(|| serde_json::json!({ "type": "object" })),
            // Fail-safe default: an undeclared stance means NOT
            // read-only, which routes dispatch through Ask.
            read_only,
        };
        self.entries
            .write()
            .expect("compose tool table poisoned")
            .insert(name.to_string(), entry);
        self.tool(name)
    }

    /// Forget one tool. The seat registration goes with the entry's fork;
    /// this is the read plane catching up.
    pub fn withdraw(&self, name: &str) {
        self.entries
            .write()
            .expect("compose tool table poisoned")
            .remove(name);
    }
}

/// Read plane only.
///
/// Registration is Rust-to-Rust and goes through [`ComposeToolRegistry::declare`]
/// and the kernel's tool seat; what is left here is the one question a plugin
/// on the other side of the pipe still asks over JSON — what is offered.
impl JsonService for ComposeToolRegistry {
    fn call(&self, method: &str, params: Value) -> Result<Value, KernelError> {
        let _ = params;
        match method {
            "list" => Ok(serde_json::json!({ "pluginTools": self.list_rows() })),
            other => Err(KernelError::Other(format!(
                "tool-registry (composition) has no method `{other}`"
            ))),
        }
    }
}

/// Proxy [`Tool`] for one composition-hosted dsh tool. Passes the engine's
/// standard validation/permission pipeline; `call` dispatches into the
/// compose isolate over the serve plane and folds the dsh `ContentBlock[]`
/// result into model-facing text.
struct ComposeDshTool {
    name: String,
    entry: ComposeToolEntry,
    registry: std::sync::Weak<ComposeToolRegistry>,
}

/// Fold dsh content blocks into one model-facing string; non-text blocks
/// degrade to a bracketed marker (same shape dsh uses when flattening).
fn fold_content_blocks(content: &Value) -> String {
    let Some(blocks) = content.as_array() else {
        return content.to_string();
    };
    blocks
        .iter()
        .map(|block| match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => block
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            Some(other) => format!("[{other} content]"),
            None => block.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for ComposeDshTool {
    fn id(&self) -> ToolId {
        ToolId::new(&self.name)
    }

    fn description(&self) -> &str {
        &self.entry.description
    }

    fn input_schema(&self) -> ToolInputSchema {
        self.entry.input_schema.clone()
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        self.entry.read_only
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        self.is_read_only(input)
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        !self.entry.read_only
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        if self.entry.read_only {
            return Ok(PermissionDecision::allow(input.clone()));
        }
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Call plugin tool",
                format!("Call composition-registered tool {}", self.name),
            )
            .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        let plane = self
            .registry
            .upgrade()
            .and_then(|registry| {
                registry
                    .plane
                    .read()
                    .expect("compose tool plane poisoned")
                    .clone()
            })
            .filter(|plane| !plane.has_exited())
            .ok_or_else(|| ToolError::Execution {
                tool: ToolId::new(&self.name),
                source: anyhow::anyhow!("the plugin composition serving this tool is not running"),
            })?;
        let envelope = plane
            .serve(&self.name, input, COMPOSE_TOOL_TIMEOUT)
            .await
            .map_err(|err| ToolError::Execution {
                tool: ToolId::new(&self.name),
                source: anyhow::anyhow!("composition tool failed: {err}"),
            })?;
        if let Some(err) = envelope.get("err").and_then(|e| e.as_str()) {
            return Err(ToolError::Execution {
                tool: ToolId::new(&self.name),
                source: anyhow::anyhow!("{err}"),
            });
        }
        let Some(ok) = envelope.get("ok") else {
            return Err(ToolError::Execution {
                tool: ToolId::new(&self.name),
                source: anyhow::anyhow!("composition tool returned no result envelope"),
            });
        };
        let text = fold_content_blocks(ok.get("content").unwrap_or(&Value::Null));
        if ok.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) {
            return Err(ToolError::Execution {
                tool: ToolId::new(&self.name),
                source: anyhow::anyhow!("{text}"),
            });
        }
        Ok(Value::String(text))
    }
}

/// Process slot for the active composition's tool registry. Set at compose
/// boot; CLEARED by an effect on the compose fork, so any teardown path (F1
/// thread-tail disposal included) collects it — born collected, not a bare
/// global (formal-model §1.2).
fn slot() -> &'static RwLock<Option<Arc<ComposeToolRegistry>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<ComposeToolRegistry>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

pub fn set_process_compose_tools(registry: Arc<ComposeToolRegistry>) {
    *slot().write().expect("compose tools slot poisoned") = Some(registry);
}

/// Clear only while the slot still holds `registry` (identity guard, the
/// llm-host teardown rule): a stale teardown never removes a successor.
pub fn clear_process_compose_tools(registry: &Arc<ComposeToolRegistry>) {
    let mut guard = slot().write().expect("compose tools slot poisoned");
    if guard
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, registry))
    {
        *guard = None;
    }
}

pub fn process_compose_tools() -> Option<Arc<ComposeToolRegistry>> {
    slot().read().expect("compose tools slot poisoned").clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaring_a_tool_makes_it_dispatchable_and_visible() {
        let registry = ComposeToolRegistry::new(["Read".to_string()]);

        assert!(
            registry.declare(" ", String::new(), None, false).is_none(),
            "an empty name is still refused"
        );

        let tool = registry
            .declare("todo_write", "d".to_string(), None, false)
            .expect("declares");
        assert_eq!(tool.id().as_str(), "todo_write");
        // Shadowing a builtin is allowed and warned about, not refused.
        assert!(registry
            .declare("Read", String::new(), None, true)
            .is_some());

        assert_eq!(registry.tool_names(), vec!["Read", "todo_write"]);
        let rows = registry.list_rows();
        assert_eq!(rows[1]["origin"], "composition");
    }

    /// Registration is Rust-to-Rust now; the JSON face answers reads only.
    #[test]
    fn the_json_face_no_longer_registers_anything() {
        let registry = ComposeToolRegistry::new([]);
        for method in ["register", "unregister"] {
            let err = registry
                .call(method, serde_json::json!({ "name": "Ghost" }))
                .expect_err("the write methods are gone");
            assert!(err.to_string().contains(method), "{err}");
        }
        assert_eq!(registry.tool_count(), 0);
        assert_eq!(
            registry.call("list", Value::Null).expect("list answers")["pluginTools"],
            serde_json::json!([])
        );
    }

    #[test]
    fn withdrawing_removes_exactly_one_tool() {
        let registry = ComposeToolRegistry::new([]);
        registry.declare("a", String::new(), None, false);
        registry.declare("b", String::new(), None, false);
        registry.withdraw("a");
        assert_eq!(registry.tool_names(), vec!["b".to_string()]);
    }

    #[test]
    fn process_slot_clear_is_identity_guarded() {
        let a = ComposeToolRegistry::new([]);
        let b = ComposeToolRegistry::new([]);
        set_process_compose_tools(a.clone());
        // A stale clear (older registry) leaves the successor in place.
        set_process_compose_tools(b.clone());
        clear_process_compose_tools(&a);
        assert!(
            process_compose_tools().is_some_and(|current| Arc::ptr_eq(&current, &b)),
            "successor survives a stale clear"
        );
        clear_process_compose_tools(&b);
        assert!(process_compose_tools().is_none());
    }

    #[tokio::test]
    async fn unbound_plane_fails_calls_loudly() {
        let registry = ComposeToolRegistry::new([]);
        let tool = registry
            .declare("t", String::new(), None, false)
            .expect("declared");
        let err = tool
            .call(serde_json::json!({}), &ToolContext::default())
            .await
            .expect_err("no plane bound");
        assert!(
            err.to_string().contains("not running"),
            "loud, actionable error: {err}"
        );
    }
}
