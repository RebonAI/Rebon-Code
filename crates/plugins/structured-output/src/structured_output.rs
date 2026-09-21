use async_trait::async_trait;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult};
use serde_json::{json, Value};

pub use rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME;

#[derive(Debug, Clone, Default)]
pub struct StructuredOutputTool;

#[async_trait]
impl Tool for StructuredOutputTool {
    fn id(&self) -> ToolId {
        ToolId::new(STRUCTURED_OUTPUT_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Return the final structured JSON value for a workflow sub-agent."
    }

    fn model_description(&self) -> &str {
        "Return exactly one final JSON object for the parent workflow runtime. The object must match the JSON schema included in your system prompt."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "description": "Final structured output object. The exact required shape is supplied in the workflow sub-agent system prompt.",
            "additionalProperties": true
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        // When the workflow runtime supplied a schema for this
        // sub-agent, validate the returned shape at the tool boundary
        // and reject mismatches with `Err` so the model sees an error
        // tool_result and calls `StructuredOutput` again with a
        // corrected object (RFC workflow-v2 §1.5 steps 5–7). Without a
        // channel (non-workflow callers) the tool stays permissive.
        if let Some(channel) = context.structured_output_channel() {
            channel.record(input.clone()).map_err(|reason| ToolError::InvalidInput {
                tool: self.id(),
                reason: format!(
                    "{reason}. Call StructuredOutput again with a single JSON object that matches the schema in your system prompt."
                ),
                error_code: None,
            })?;
        }
        Ok(json!({ "accepted": true, "value": input }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::{validate_structured_output, StructuredOutputChannel};
    use std::sync::Arc;

    fn demo_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "score": { "type": "number" },
                "notes": { "type": "string" }
            },
            "required": ["score", "notes"],
            "additionalProperties": false
        })
    }

    #[test]
    fn validate_structured_output_flags_top_level_type_mismatch() {
        let err = validate_structured_output(&json!({ "type": "object" }), &Value::Null)
            .expect_err("null should not satisfy top-level object schema");
        assert!(
            err.contains("schema type"),
            "error should name schema type: {err}"
        );
    }

    #[test]
    fn validate_structured_output_flags_missing_required() {
        let err = validate_structured_output(&demo_schema(), &json!({ "score": 1 }))
            .expect_err("missing `notes` should fail");
        assert!(err.contains("notes"), "error should name the field: {err}");
    }

    #[test]
    fn validate_structured_output_flags_type_mismatch() {
        let err =
            validate_structured_output(&demo_schema(), &json!({ "score": "high", "notes": "x" }))
                .expect_err("string score should fail the number type");
        assert!(err.contains("score"), "error should name the field: {err}");
    }

    #[test]
    fn validate_structured_output_accepts_valid_shape() {
        assert!(validate_structured_output(
            &demo_schema(),
            &json!({ "score": 9, "notes": "looks good" }),
        )
        .is_ok());
    }

    #[tokio::test]
    async fn call_rejects_bad_shape_and_records_valid_one() {
        let tool = StructuredOutputTool;
        let channel = Arc::new(StructuredOutputChannel::new(Some(demo_schema())));
        let ctx = ToolContext::new().with_structured_output_channel(channel.clone());

        // Wrong shape -> Err so the model retries; nothing recorded yet.
        let bad = tool.call(json!({ "score": 1 }), &ctx).await;
        assert!(bad.is_err(), "missing required field must be rejected");
        assert!(!channel.is_satisfied());

        // Correct shape -> Ok and recorded as the accepted value.
        let good = json!({ "score": 9, "notes": "ok" });
        let ok = tool.call(good.clone(), &ctx).await.expect("valid shape");
        assert_eq!(ok["accepted"], json!(true));
        assert!(channel.is_satisfied());
        assert_eq!(channel.accepted(), Some(good));
    }

    #[tokio::test]
    async fn call_without_channel_stays_permissive() {
        let tool = StructuredOutputTool;
        let ctx = ToolContext::new();
        let out = tool
            .call(json!({ "anything": [1, 2, 3] }), &ctx)
            .await
            .expect("no channel means no validation");
        assert_eq!(out["accepted"], json!(true));
    }

    #[tokio::test]
    async fn channel_boundaries_preserve_valid_results_and_isolate_workers() {
        let channel = Arc::new(StructuredOutputChannel::new(Some(
            json!({"type": "integer"}),
        )));
        let context = ToolContext::new().with_structured_output_channel(channel.clone());
        let other = Arc::new(StructuredOutputChannel::new(None));
        let other_context = ToolContext::new().with_structured_output_channel(other.clone());
        StructuredOutputTool
            .call(json!(u64::MAX), &context.clone())
            .await
            .unwrap();
        assert!(StructuredOutputTool
            .call(json!(1.5), &context)
            .await
            .is_err());
        assert_eq!(channel.accepted(), Some(json!(u64::MAX)));
        assert!(!other.is_satisfied());
        StructuredOutputTool
            .call(Value::Null, &other_context)
            .await
            .unwrap();
        assert_eq!(other.accepted(), Some(Value::Null));
        StructuredOutputTool
            .call(json!(i64::MIN), &context)
            .await
            .unwrap();
        assert_eq!(channel.accepted(), Some(json!(i64::MIN)));
    }
}
