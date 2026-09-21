//! What a session offers a model beyond its builtins.
//!
//! One layer now: the session's own `run_code` (rebon-native).
//! `Engine::invoke_tool` reaches it through [`PluginToolProvider`], so it
//! passes validation and the permission broker exactly like a builtin or an
//! MCP tool, and precedence stays builtin > MCP > plugin by fallback order.
//!
//! There were two more, and both went the same way — into the kernel's tool
//! seat.
//!
//! The composition's tools were a second layer here, read out of a
//! process-wide slot on every lookup. The plugin plane registers them on the
//! process seat when an entry loads and the entry's scope takes them off when
//! it unloads (RFC kernel-plugins §8), and a turn seat already consults that
//! seat as its upstream — so reading them from a slot as well would be the
//! same tools by a second route. What is left of the slot is the read plane:
//! `list` still names them, because a plugin on the other side of the pipe
//! asks what is offered.
//!
//! The third was older: a plugin could call `tool-registry register` with the
//! path of a module, and rebon ran that module on a persistent V8 isolate of
//! its own. Nothing calls it any more — the only JS that ever did was the
//! embedded composition — and a plugin on the plane declares its tools in its
//! manifest and serves them itself, which is the same capability without rebon
//! executing a file a plugin named.

use std::sync::{Arc, RwLock};

use rebon_core::Engine;
use rebon_kernel::{JsonService, KernelError};
use rebon_tool::{PluginToolProvider, Tool};
use serde_json::Value;

/// Session-scoped view of what a model may call: the engine's catalog and the
/// session's `run_code`.
pub struct SessionPluginTools {
    engine: Arc<Engine>,
    /// The session's `run_code` tool (PTC / Code Mode), when enabled.
    /// rebon-native, so it outranks a same-name composition tool; the
    /// engine's builtins still outrank it structurally.
    run_code: RwLock<Option<Arc<dyn Tool>>>,
}

impl SessionPluginTools {
    pub fn new(engine: Arc<Engine>) -> Arc<Self> {
        Arc::new(Self {
            engine,
            run_code: RwLock::new(None),
        })
    }

    /// Attach the session's Code Mode tool (bootstrap wiring).
    pub fn set_run_code(&self, tool: Arc<dyn Tool>) {
        *self.run_code.write().expect("run_code slot poisoned") = Some(tool);
    }

    fn list(&self) -> Value {
        let tools: Vec<Value> = self
            .engine
            .eager_tool_snapshots()
            .into_iter()
            .map(|snapshot| {
                serde_json::json!({
                    "name": snapshot.name,
                    "description": snapshot.description,
                    "inputSchema": snapshot.input_schema,
                })
            })
            .collect();
        let mut plugin_tools: Vec<Value> = Vec::new();
        if let Some(registry) = crate::kernel_compose_tools::process_compose_tools() {
            plugin_tools.extend(registry.list_rows());
        }
        plugin_tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        serde_json::json!({
            "tools": tools,
            "deferred": self.engine.deferred_tool_names(),
            "pluginTools": plugin_tools,
        })
    }
}

impl JsonService for SessionPluginTools {
    fn call(&self, method: &str, params: Value) -> Result<Value, KernelError> {
        let _ = params;
        match method {
            "list" => Ok(self.list()),
            other => Err(KernelError::Other(format!(
                "tool-registry has no method `{other}`"
            ))),
        }
    }
}

/// What this session brings that the process seat does not: `run_code`.
///
/// The composition's tools used to be here too, fetched from a process slot
/// on every lookup. They are on the kernel's process tool seat now — the
/// plugin plane registers them there when an entry loads and the entry's
/// scope takes them off when it unloads — and a turn seat already consults
/// that seat as its upstream. Reading them from a slot as well would be the
/// same tools by a second route, which is one more place for "what does this
/// session offer" to have two answers.
impl PluginToolProvider for SessionPluginTools {
    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.run_code
            .read()
            .expect("run_code slot poisoned")
            .clone()
            .filter(|tool| tool.id().as_str() == name)
    }

    fn tool_names(&self) -> Vec<String> {
        self.run_code
            .read()
            .expect("run_code slot poisoned")
            .as_ref()
            .map(|tool| vec![tool.id().as_str().to_string()])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Arc<Engine> {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(rebon_tool::AgentRegistry::load_with_plugin_dirs(
            dir.path(),
            &dir.path().join("config"),
            &[],
        ));
        rebon_tool::set_agent_registry_selection(registry, false);
        Arc::new(Engine::with_builtin_tools())
    }

    /// Registering a tool by naming a module is gone, and says so.
    ///
    /// It used to mean "run this file on an isolate rebon keeps for you". A
    /// plugin on the plane declares its tools in its manifest and serves them
    /// itself, so the capability survives without rebon executing a path a
    /// plugin handed it. A caller that still tries gets the method named back,
    /// not a silent success that registers nothing.
    #[test]
    fn a_plugin_can_no_longer_hand_rebon_a_module_to_execute() {
        let tools = SessionPluginTools::new(engine());
        for method in ["register", "unregister"] {
            let err = tools
                .call(
                    method,
                    serde_json::json!({ "name": "Ghost", "module": "Z:/does/not/exist.js" }),
                )
                .expect_err("the method is gone");
            assert!(err.to_string().contains(method), "{err}");
        }
        assert!(tools.tool("Ghost").is_none());
    }

    /// The read plane is the engine's catalog plus whatever the composition
    /// registered — and with no composition running, exactly the former.
    #[test]
    fn the_read_plane_is_the_engine_catalog_plus_the_composition() {
        let tools = SessionPluginTools::new(engine());
        let listed = tools.call("list", Value::Null).expect("list answers");
        assert!(
            listed["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()),
            "the engine's own tools are the floor: {listed}"
        );
        assert!(listed["deferred"].is_array(), "{listed}");
        assert_eq!(
            listed["pluginTools"],
            serde_json::json!([]),
            "nothing is offered that nothing can run"
        );
        assert!(tools.tool_names().is_empty());
    }
}
