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
/// Codex's `background_terminal_max_timeout` default: long enough that one
/// wait can cover a whole build instead of a string of short polls.
const MAX_WAIT_TIMEOUT_MS: u64 = 300_000;
/// Codex's `MIN_EMPTY_YIELD_TIME_MS`: a wait shorter than this is almost
/// always a poll loop in the making, and each poll resends the conversation.
const MIN_WAIT_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct ShellOutputTool;

#[derive(Debug, Clone, Copy, Default)]
pub struct ShellStopTool;

#[derive(Debug)]
struct ShellOutputInput {
    shell_id: Option<String>,
    wait: bool,
    timeout_ms: u64,
}

#[async_trait]
impl Tool for ShellOutputTool {
    fn id(&self) -> ToolId {
        ToolId::new(SHELL_OUTPUT_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Lists background Bash/PowerShell shells, or waits for one to exit. Omit shellId to list \
         the shells of this session/agent with their status and outputPath. With shellId, blocks \
         until that shell exits or `timeout` passes, then returns its status, exit code, the last \
         lines of its output (`outputTail`) and `outputPath`. It returns on exit, not on new \
         output: to wait for a build or test run, make one call with a long timeout rather than \
         many short ones. wait=false returns the status at once. A shell's full output is the \
         file at outputPath: read it with Read or search it with Grep. Monitor tasks are not \
         handled here: their events arrive as task notifications."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "shellId": {
                    "type": "string",
                    "description": "Background shell ID. Omit to list shells."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Longest to wait for the shell to exit, in milliseconds. Default 30000; values are kept within 5000 to 300000."
                },
                "wait": {
                    "type": "boolean",
                    "description": "Default true. false returns the current status at once instead of waiting."
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
                    .wait(
                        context,
                        SHELL_OUTPUT_TOOL_NAME,
                        &shell_id,
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
    // Every combination below means one thing, so none is refused: a
    // timeout alone is a wait, a timeout with wait=false is a status check,
    // and an out-of-range timeout is the nearest one in range. Rejecting
    // these cost a round-trip each time a model wrote the natural call.
    let wait = optional_bool(object, "wait", SHELL_OUTPUT_TOOL_NAME)?.unwrap_or(true);
    let timeout_ms = optional_u64(object, "timeout", SHELL_OUTPUT_TOOL_NAME)?
        .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
        .clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS);

    Ok(ShellOutputInput {
        shell_id,
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

    /// A timeout on its own is the call models write most; it means "wait
    /// this long", and refusing it was the whole bug.
    #[test]
    fn shell_output_treats_a_timeout_alone_as_a_wait() {
        let parsed =
            parse_shell_output_input(&json!({ "shellId": "sh_test", "timeout": 120_000 })).unwrap();
        assert!(parsed.wait);
        assert_eq!(parsed.timeout_ms, 120_000);
    }

    #[test]
    fn shell_output_waits_by_default() {
        let parsed = parse_shell_output_input(&json!({ "shellId": "sh_test" })).unwrap();
        assert!(parsed.wait);
        assert_eq!(parsed.timeout_ms, DEFAULT_WAIT_TIMEOUT_MS);
    }

    #[test]
    fn shell_output_wait_false_is_a_status_check_even_with_a_timeout() {
        let parsed = parse_shell_output_input(
            &json!({ "shellId": "sh_test", "wait": false, "timeout": 60_000 }),
        )
        .unwrap();
        assert!(!parsed.wait);
    }

    #[test]
    fn shell_output_keeps_timeouts_in_range_instead_of_refusing_them() {
        for (asked, kept) in [
            (0, MIN_WAIT_TIMEOUT_MS),
            (100, MIN_WAIT_TIMEOUT_MS),
            (MAX_WAIT_TIMEOUT_MS + 1, MAX_WAIT_TIMEOUT_MS),
            (u64::MAX, MAX_WAIT_TIMEOUT_MS),
        ] {
            let parsed =
                parse_shell_output_input(&json!({ "shellId": "sh_test", "timeout": asked }))
                    .unwrap();
            assert_eq!(parsed.timeout_ms, kept, "timeout {asked}");
        }
    }

    #[tokio::test]
    async fn shell_output_still_rejects_wrongly_typed_fields() {
        for input in [
            json!({ "shellId": "sh_test", "timeout": "soon" }),
            json!({ "shellId": "sh_test", "wait": "yes" }),
            json!({ "shellId": 7 }),
        ] {
            let result = ShellOutputTool
                .validate_input(&input, &ToolContext::new())
                .await
                .unwrap();
            assert!(!result.is_valid(), "{input}");
        }
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
