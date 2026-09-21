use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use rebon_tools_core::{ToolError, ToolId, ToolInputSchema};

use crate::{Tool, ToolContext, ToolResult};

pub const INVOKE_DEFERRED_TOOL_NAME: &str = "InvokeDeferredTool";

#[derive(Debug, Deserialize)]
pub struct InvokeDeferredToolInput {
    pub tool_name: String,
    pub arguments: Value,
}

/// Stable provider-visible gateway for executing tools discovered through ToolSearch.
///
/// Runtime dispatch is special-cased by the run loop's `invoke_tool` so the
/// real deferred tool still goes through the run loop's schema validation,
/// tool-local validation, permission checks, and execution path.
#[derive(Clone, Default)]
pub struct InvokeDeferredTool;

#[async_trait]
impl Tool for InvokeDeferredTool {
    fn id(&self) -> ToolId {
        ToolId::new(INVOKE_DEFERRED_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Invoke a deferred tool discovered through ToolSearch."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "tool_name": {
                    "type": "string",
                    "description": "ToolSearch name."
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments object matching the target tool's schema. Must contain all fields required by that tool. Pass {} explicitly only when the target tool takes no parameters."
                }
            },
            "required": ["tool_name", "arguments"]
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
        Err(ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("InvokeDeferredTool must be dispatched by the engine gateway"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_schema_requires_arguments_without_default() {
        let tool = InvokeDeferredTool;
        assert_eq!(tool.id().as_str(), "InvokeDeferredTool");
        let schema = tool.input_schema();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(
            schema["properties"]["tool_name"]["description"],
            json!("ToolSearch name.")
        );
        assert_eq!(
            schema["properties"]["arguments"]["description"],
            json!("Arguments object matching the target tool's schema. Must contain all fields required by that tool. Pass {} explicitly only when the target tool takes no parameters.")
        );
        assert!(schema["properties"]["arguments"].get("default").is_none());
        assert_eq!(schema["required"], json!(["tool_name", "arguments"]));
        assert!(schema["properties"]["tool_name"].get("enum").is_none());
    }

    #[test]
    fn gateway_input_rejects_missing_arguments() {
        let err = serde_json::from_value::<InvokeDeferredToolInput>(json!({
            "tool_name": "Sleep"
        }))
        .unwrap_err();

        assert!(err.to_string().contains("missing field `arguments`"));
    }
}
