use crate::{Tool, ToolContext};
use async_trait::async_trait;
use rebon_tools_core::{
    parse_tool_input, ToolId, ToolInputSchema, ToolProgressUpdate, ToolResult, ValidationOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration, Instant};

const SLEEP_TOOL_NAME: &str = "Sleep";
const INVALID_INPUT_CODE: i64 = 400;
const MAX_SLEEP_MS: u64 = 300_000;

#[derive(Debug, Clone, Default)]
pub struct SleepTool;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepInput {
    duration_ms: u64,
}

#[async_trait]
impl Tool for SleepTool {
    fn id(&self) -> ToolId {
        ToolId::new(SLEEP_TOOL_NAME)
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("wait pause delay timer rest idle")
    }

    fn description(&self) -> &str {
        "Wait for a specified duration. The user can interrupt the sleep at any time.\n\
         \n\
         Use this when the user tells you to sleep or rest, when you have nothing to do, \
         or when you're waiting for something.\n\
         \n\
         You can call this concurrently with other tools \u{2014} it won't interfere with them.\n\
         \n\
         Prefer this over `Bash(sleep ...)` \u{2014} it doesn't hold a shell process."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "duration_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How long to wait in milliseconds"
                }
            },
            "required": ["duration_ms"],
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
        // Schema-level validation (missing fields, type mismatches)
        // is already handled by the engine's centralized validator.
        // Here we only do semantic checks.
        let parsed: SleepInput = parse_tool_input(self.id(), input)?;
        if parsed.duration_ms > MAX_SLEEP_MS {
            return Ok(ValidationOutcome::invalid(
                format!("`duration_ms` exceeds max supported sleep of {MAX_SLEEP_MS}ms"),
                INVALID_INPUT_CODE,
            ));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        let parsed: SleepInput = parse_tool_input(self.id(), &input)?;

        _context.emit_progress(
            ToolProgressUpdate::new("sleep_start")
                .with_message(format!("sleeping {}ms", parsed.duration_ms)),
        );
        let start = Instant::now();
        sleep(Duration::from_millis(parsed.duration_ms)).await;
        let elapsed = start.elapsed().as_millis() as u64;
        _context.emit_progress(
            ToolProgressUpdate::new("sleep_end").with_message(format!("slept {}ms", elapsed)),
        );

        Ok(json!({
            "durationMs": elapsed,
            "completed": true,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> SleepTool {
        SleepTool
    }

    #[tokio::test]
    async fn validate_input_rejects_large_sleep() {
        let result = tool()
            .validate_input(
                &json!({ "duration_ms": MAX_SLEEP_MS + 1 }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn call_waits_and_reports_elapsed_time() {
        let (context, mut rx) = ToolContext::new().with_progress("sleep-1");
        let out = tool()
            .call(json!({ "duration_ms": 20 }), &context)
            .await
            .unwrap();

        let first = rx.recv().await.expect("expected start progress");
        let second = rx.recv().await.expect("expected end progress");
        assert_eq!(first.kind, "sleep_start");
        assert_eq!(second.kind, "sleep_end");
        assert_eq!(out["completed"], json!(true));
        assert!(out["durationMs"].as_u64().unwrap() >= 20);
    }
}
