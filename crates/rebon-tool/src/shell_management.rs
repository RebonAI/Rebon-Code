use crate::{ShellProcessRegistry, Tool, ToolContext};
use async_trait::async_trait;
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub const SHELL_OUTPUT_TOOL_NAME: &str = "ShellOutput";
pub const SHELL_STOP_TOOL_NAME: &str = "ShellStop";
const INVALID_INPUT_CODE: i64 = 400;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct ShellOutputTool;

#[derive(Debug, Clone, Copy, Default)]
pub struct ShellStopTool;

#[derive(Debug)]
struct ShellOutputInput {
    shell_id: Option<String>,
    cursor: u64,
    wait: bool,
    timeout_ms: u64,
}

#[async_trait]
impl Tool for ShellOutputTool {
    fn id(&self) -> ToolId {
        ToolId::new(SHELL_OUTPUT_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Lists background Bash/PowerShell shells or reads their incremental output. Omit shellId \
         to list shells visible in the current session/agent. With shellId, new text arrives in \
         `output` (stdout) and `stderr`; pass the returned nextCursor to avoid duplicate output. \
         wait=true waits for new output or process completion."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "shellId": {
                    "type": "string",
                    "description": "Background shell ID. Omit to list visible shells."
                },
                "cursor": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Incremental output cursor returned by a previous call."
                },
                "wait": {
                    "type": "boolean",
                    "description": "Wait for new output or a terminal process state."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_WAIT_TIMEOUT_MS,
                    "description": "Maximum wait time in milliseconds; only valid with wait=true."
                }
            },
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_shell_output_input(input))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_shell_output_input(&input)?;
        let registry = registry(context, SHELL_OUTPUT_TOOL_NAME)?;
        match parsed.shell_id {
            Some(shell_id) => {
                registry
                    .output(
                        context,
                        SHELL_OUTPUT_TOOL_NAME,
                        &shell_id,
                        parsed.cursor,
                        parsed.wait,
                        parsed.timeout_ms,
                    )
                    .await
            }
            None => registry.list(context, SHELL_OUTPUT_TOOL_NAME),
        }
    }
}

#[async_trait]
impl Tool for ShellStopTool {
    fn id(&self) -> ToolId {
        ToolId::new(SHELL_STOP_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Stops a background Bash or PowerShell process tree by shellId. Repeated calls are \
         idempotent, and stopping an already completed shell is a successful no-op."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "shellId": { "type": "string" }
            },
            "required": ["shellId"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_shell_id(input, SHELL_STOP_TOOL_NAME))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let shell_id = parse_shell_id(&input, SHELL_STOP_TOOL_NAME)?;
        registry(context, SHELL_STOP_TOOL_NAME)?.stop(context, SHELL_STOP_TOOL_NAME, &shell_id)
    }
}

fn parse_shell_output_input(input: &Value) -> ToolResult<ShellOutputInput> {
    let object = object_input(input, SHELL_OUTPUT_TOOL_NAME)?;
    let shell_id = optional_non_empty_string(object, "shellId", SHELL_OUTPUT_TOOL_NAME)?;
    let has_cursor = object.contains_key("cursor");
    let has_wait = object.contains_key("wait");
    let has_timeout = object.contains_key("timeout");

    if shell_id.is_none() && (has_cursor || has_wait || has_timeout) {
        return Err(invalid_input(
            SHELL_OUTPUT_TOOL_NAME,
            "`cursor`, `wait`, and `timeout` require `shellId`",
        ));
    }

    let cursor = optional_u64(object, "cursor", SHELL_OUTPUT_TOOL_NAME)?.unwrap_or(0);
    let wait = optional_bool(object, "wait", SHELL_OUTPUT_TOOL_NAME)?.unwrap_or(false);
    let timeout_ms =
        optional_u64(object, "timeout", SHELL_OUTPUT_TOOL_NAME)?.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);

    if has_timeout && !wait {
        return Err(invalid_input(
            SHELL_OUTPUT_TOOL_NAME,
            "`timeout` is only valid when `wait` is true",
        ));
    }
    if timeout_ms == 0 || timeout_ms > MAX_WAIT_TIMEOUT_MS {
        return Err(invalid_input(
            SHELL_OUTPUT_TOOL_NAME,
            format!("`timeout` must be between 1 and {MAX_WAIT_TIMEOUT_MS}ms"),
        ));
    }

    Ok(ShellOutputInput {
        shell_id,
        cursor,
        wait,
        timeout_ms,
    })
}

fn parse_shell_id(input: &Value, tool_name: &str) -> ToolResult<String> {
    let object = object_input(input, tool_name)?;
    optional_non_empty_string(object, "shellId", tool_name)?
        .ok_or_else(|| invalid_input(tool_name, format!("{tool_name} requires `shellId`")))
}

fn object_input<'a>(input: &'a Value, tool_name: &str) -> ToolResult<&'a Map<String, Value>> {
    input
        .as_object()
        .ok_or_else(|| invalid_input(tool_name, format!("{tool_name} input must be an object")))
}

fn optional_non_empty_string(
    object: &Map<String, Value>,
    field: &str,
    tool_name: &str,
) -> ToolResult<Option<String>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.trim().to_owned())),
        Some(Value::String(_)) => Err(invalid_input(
            tool_name,
            format!("`{field}` must not be empty"),
        )),
        Some(_) => Err(invalid_input(
            tool_name,
            format!("`{field}` must be a string when provided"),
        )),
    }
}

fn optional_u64(
    object: &Map<String, Value>,
    field: &str,
    tool_name: &str,
) -> ToolResult<Option<u64>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_u64().map(Some).ok_or_else(|| {
            invalid_input(
                tool_name,
                format!("`{field}` must be a non-negative integer"),
            )
        }),
        Some(_) => Err(invalid_input(
            tool_name,
            format!("`{field}` must be an integer when provided"),
        )),
    }
}

fn optional_bool(
    object: &Map<String, Value>,
    field: &str,
    tool_name: &str,
) -> ToolResult<Option<bool>> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(invalid_input(
            tool_name,
            format!("`{field}` must be a boolean when provided"),
        )),
    }
}

fn registry<'a>(
    context: &'a ToolContext,
    tool_name: &str,
) -> ToolResult<&'a Arc<ShellProcessRegistry>> {
    context
        .shell_process_registry()
        .ok_or_else(|| ToolError::Execution {
            tool: ToolId::new(tool_name),
            source: anyhow::anyhow!("background shell registry is unavailable"),
        })
}

fn invalid_input(tool_name: &str, reason: impl Into<String>) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(tool_name),
        reason: reason.into(),
        error_code: Some(INVALID_INPUT_CODE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shell_output_accepts_empty_input_for_listing() {
        let result = ShellOutputTool
            .validate_input(&json!({}), &ToolContext::new())
            .await
            .unwrap();
        assert!(result.is_valid());
    }

    #[tokio::test]
    async fn shell_output_rejects_management_fields_without_shell_id() {
        let result = ShellOutputTool
            .validate_input(&json!({ "wait": true }), &ToolContext::new())
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn shell_output_rejects_timeout_without_wait() {
        let result = ShellOutputTool
            .validate_input(
                &json!({ "shellId": "sh_test", "timeout": 100 }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn shell_output_rejects_wait_timeout_above_limit() {
        let result = ShellOutputTool
            .validate_input(
                &json!({
                    "shellId": "sh_test",
                    "wait": true,
                    "timeout": MAX_WAIT_TIMEOUT_MS + 1
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn shell_stop_requires_non_empty_shell_id() {
        let result = ShellStopTool
            .validate_input(&json!({ "shellId": " " }), &ToolContext::new())
            .await
            .unwrap();
        assert!(!result.is_valid());
    }
}
