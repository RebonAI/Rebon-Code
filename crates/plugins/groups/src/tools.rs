//! The group tools as a Rebon session's own tools.
//!
//! Each one is `rebon-group`'s schema and handler under the tool trait. The
//! caller is never in doubt here: it is the session running the tool, so
//! nothing is read from the environment and `group_join` needs no agent or
//! session id.

use async_trait::async_trait;
use rebon_group::identity::{AgentKind, Caller};
use rebon_group::tools::{self as group_tools, ToolContext as GroupContext, ToolSpec};
use rebon_group::GroupStore;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult};
use serde_json::Value;

/// One group tool.
pub struct GroupTool {
    spec: ToolSpec,
    store: GroupStore,
}

impl GroupTool {
    pub fn new(spec: ToolSpec, store: GroupStore) -> Self {
        Self { spec, store }
    }
}

/// Every group tool over `store`.
pub fn tools(store: &GroupStore) -> Vec<GroupTool> {
    group_tools::specs()
        .into_iter()
        .map(|spec| GroupTool::new(spec, store.clone()))
        .collect()
}

#[async_trait]
impl Tool for GroupTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.spec.name)
    }

    fn description(&self) -> &str {
        self.spec.description
    }

    fn input_schema(&self) -> ToolInputSchema {
        self.spec.input_schema.clone()
    }

    /// Loaded on demand like MCP tools: seven schemas the model needs only
    /// once the session is in a group, and the delivery attachment names the
    /// ones to reach for.
    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("agent group other agents messages shared memory team coordinate")
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        self.spec.read_only
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        self.spec.read_only
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let Some(session_id) = context.session_id().map(str::to_string) else {
            return Err(self.failed("this session has no id to join a group under"));
        };
        let root = match context.cwd() {
            Some(cwd) => cwd.to_string(),
            None => std::env::current_dir()
                .map(|dir| dir.to_string_lossy().into_owned())
                .map_err(|error| self.failed(format!("no working directory: {error}")))?,
        };
        let mut caller = Some(Caller {
            agent: AgentKind::REBON.to_string(),
            session_id,
        });
        group_tools::call(
            &mut GroupContext {
                store: &self.store,
                caller: &mut caller,
                root: &root,
            },
            self.spec.name,
            input,
        )
        .map_err(|error| self.failed(format!("{error:#}")))
    }
}

impl GroupTool {
    fn failed(&self, message: impl Into<String>) -> ToolError {
        ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!(message.into()),
        }
    }
}
