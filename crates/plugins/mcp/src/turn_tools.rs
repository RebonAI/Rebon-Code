//! The plugin's per-turn MCP tool contribution.

use crate::mcp::McpProxyTool;
use rebon_core::tool_seat::SeatToolProvider;
use rebon_tool::{McpToolDefinition, Tool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(super) struct McpToolProvider {
    pub(super) definitions: Vec<(String, McpToolDefinition)>,
    pub(super) active: Arc<AtomicBool>,
}

impl SeatToolProvider for McpToolProvider {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if !self.active.load(Ordering::Acquire) {
            return None;
        }
        self.definitions
            .iter()
            .find(|(server, definition)| {
                rebon_tool::build_mcp_tool_name(server, &definition.name) == name
            })
            .map(|(server, definition)| {
                Arc::new(
                    McpProxyTool::new(server.clone(), definition.clone())
                        .with_liveness(self.active.clone()),
                ) as Arc<dyn Tool>
            })
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if !self.active.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.definitions
            .iter()
            .map(|(server, definition)| {
                Arc::new(
                    McpProxyTool::new(server.clone(), definition.clone())
                        .with_liveness(self.active.clone()),
                ) as Arc<dyn Tool>
            })
            .collect()
    }
}
