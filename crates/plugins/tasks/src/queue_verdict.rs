use async_trait::async_trait;
use rebon_tool::{QueueVerdict, QueueVerdictOutcome, Tool, ToolContext};
use rebon_tools_core::{
    parse_tool_input, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub const QUEUE_VERDICT_TOOL_NAME: &str = "QueueVerdict";
const INVALID_INPUT_CODE: i64 = 400;
const MAX_REASON_CHARS: usize = 2_000;

/// Record a review outcome for one task line.
///
/// Replaces reading a verdict back out of the coordinator's own prose. A
/// verdict that has to be pattern-matched out of a reply is lost whenever the
/// reply is phrased differently, and it carries no way to tell which execution
/// of the row it was about — so a verdict written about round 1 could land on
/// round 2. Submitting it as data fixes both.
#[derive(Debug, Clone, Default)]
pub struct QueueVerdictTool;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct QueueVerdictInput {
    row_id: String,
    generation: u64,
    pass: bool,
    #[serde(default)]
    reason: String,
}

#[async_trait]
impl Tool for QueueVerdictTool {
    fn id(&self) -> ToolId {
        ToolId::new(QUEUE_VERDICT_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Record the review outcome for one task line of the Agent Queue you \
         are coordinating.\n\
         \n\
         Pass `generation` exactly as `QueuePlan` reported it for that row. It \
         identifies which execution you judged: if the row has since started \
         another round, your verdict is rejected instead of being applied to \
         work you never saw.\n\
         \n\
         `pass: false` sends the row back for rework and your `reason` becomes \
         the instruction the next round is given, so state what is missing, \
         not that it failed.\n\
         \n\
         Judge the work before calling this — inspect the diff against the \
         row's review baseline. This tool records a conclusion; it does not \
         reach one."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "rowId": {
                    "type": "string",
                    "description": "Row id from QueuePlan (the `id` field, not the line number)"
                },
                "generation": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The row's `generation` from QueuePlan — the execution you judged"
                },
                "pass": {
                    "type": "boolean",
                    "description": "true accepts this round; false sends it back for rework"
                },
                "reason": {
                    "type": "string",
                    "description": "Why. On a rework this is handed to the next round as its instruction."
                }
            },
            "required": ["rowId", "generation", "pass"],
            "additionalProperties": false
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let parsed: QueueVerdictInput = parse_tool_input(self.id(), input)?;
        if parsed.row_id.trim().is_empty() {
            return Ok(ValidationOutcome::invalid(
                "`rowId` is required".to_string(),
                INVALID_INPUT_CODE,
            ));
        }
        if parsed.generation == 0 {
            return Ok(ValidationOutcome::invalid(
                "`generation` must come from QueuePlan and is never 0".to_string(),
                INVALID_INPUT_CODE,
            ));
        }
        // A rework reason is the next round's instruction, so an empty one
        // leaves the worker with nothing to act on.
        if !parsed.pass && parsed.reason.trim().is_empty() {
            return Ok(ValidationOutcome::invalid(
                "`reason` is required when `pass` is false: it becomes the next round's instruction"
                    .to_string(),
                INVALID_INPUT_CODE,
            ));
        }
        if parsed.reason.chars().count() > MAX_REASON_CHARS {
            return Ok(ValidationOutcome::invalid(
                format!("`reason` exceeds {MAX_REASON_CHARS} characters"),
                INVALID_INPUT_CODE,
            ));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: QueueVerdictInput = parse_tool_input(self.id(), &input)?;
        let Some(controller) = context.queue_controller() else {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "this session does not coordinate an Agent Queue".to_string(),
                error_code: Some(INVALID_INPUT_CODE),
            });
        };
        let Some(session_id) = context.session_id() else {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "this session has no id, so its queue cannot be resolved".to_string(),
                error_code: Some(INVALID_INPUT_CODE),
            });
        };
        let outcome = controller
            .submit_verdict(QueueVerdict {
                session_id,
                row_id: parsed.row_id.trim(),
                generation: parsed.generation,
                pass: parsed.pass,
                reason: parsed.reason.trim(),
            })
            .await
            .map_err(|error| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(error),
            })?;
        Ok(match outcome {
            QueueVerdictOutcome::Applied { status } => json!({
                "applied": true,
                "rowId": parsed.row_id,
                "generation": parsed.generation,
                "status": status,
            }),
            // Surfaced as a result rather than an error: a stale verdict is a
            // normal race, and the coordinator's next move is to re-read the
            // plan, not to retry the same call.
            QueueVerdictOutcome::Rejected { reason } => json!({
                "applied": false,
                "rowId": parsed.row_id,
                "generation": parsed.generation,
                "rejected": reason,
            }),
            QueueVerdictOutcome::Unconfirmed => json!({
                "applied": false,
                "rowId": parsed.row_id,
                "generation": parsed.generation,
                "unconfirmed": "submitted, but the queue did not confirm in time; \
                                re-read QueuePlan before deciding anything else",
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::QueueController;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct SpyController {
        seen: Mutex<Vec<(String, String, u64, bool, String)>>,
        outcome: Option<QueueVerdictOutcome>,
    }

    #[async_trait]
    impl QueueController for SpyController {
        async fn queue_plan(&self, _session_id: &str) -> Result<Option<Value>, String> {
            Ok(None)
        }

        async fn submit_verdict(
            &self,
            verdict: QueueVerdict<'_>,
        ) -> Result<QueueVerdictOutcome, String> {
            self.seen.lock().unwrap().push((
                verdict.session_id.to_string(),
                verdict.row_id.to_string(),
                verdict.generation,
                verdict.pass,
                verdict.reason.to_string(),
            ));
            Ok(self
                .outcome
                .clone()
                .unwrap_or(QueueVerdictOutcome::Applied {
                    status: "done".into(),
                }))
        }

        async fn dispatch_row(
            &self,
            _session_id: &str,
            _row_id: &str,
            _worktree: &str,
        ) -> Result<QueueVerdictOutcome, String> {
            unreachable!("QueueVerdict never dispatches")
        }

        async fn block_row(
            &self,
            _session_id: &str,
            _row_id: &str,
            _reason: &str,
        ) -> Result<QueueVerdictOutcome, String> {
            unreachable!("QueueVerdict never blocks")
        }
    }

    fn context_with(controller: Arc<SpyController>) -> ToolContext {
        ToolContext::new()
            .with_session_id("sess-coordinator-1")
            .with_queue_controller(controller)
    }

    #[tokio::test]
    async fn a_pass_reaches_the_writer_with_the_generation_it_judged() {
        let spy = Arc::new(SpyController::default());
        let context = context_with(spy.clone());
        let out = QueueVerdictTool
            .call(
                json!({ "rowId": "task-1", "generation": 3, "pass": true, "reason": "gates green" }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["applied"], json!(true));
        let seen = spy.seen.lock().unwrap();
        assert_eq!(
            seen[0],
            (
                "sess-coordinator-1".into(),
                "task-1".into(),
                3,
                true,
                "gates green".into()
            )
        );
    }

    #[tokio::test]
    async fn a_stale_verdict_comes_back_as_a_result_not_an_error() {
        let spy = Arc::new(SpyController {
            outcome: Some(QueueVerdictOutcome::Rejected {
                reason: "row is on generation 4".into(),
            }),
            ..Default::default()
        });
        let out = QueueVerdictTool
            .call(
                json!({ "rowId": "task-1", "generation": 3, "pass": true }),
                &context_with(spy),
            )
            .await
            .expect("a rejection is a result, so the model can re-read the plan");
        assert_eq!(out["applied"], json!(false));
        assert_eq!(out["rejected"], json!("row is on generation 4"));
    }

    #[tokio::test]
    async fn a_rework_without_a_reason_is_refused() {
        let context = context_with(Arc::new(SpyController::default()));
        let outcome = QueueVerdictTool
            .validate_input(
                &json!({ "rowId": "task-1", "generation": 2, "pass": false }),
                &context,
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());

        // A pass needs no reason.
        let outcome = QueueVerdictTool
            .validate_input(
                &json!({ "rowId": "task-1", "generation": 2, "pass": true }),
                &context,
            )
            .await
            .unwrap();
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn generation_zero_is_refused_because_the_plan_never_reports_it() {
        let context = context_with(Arc::new(SpyController::default()));
        let outcome = QueueVerdictTool
            .validate_input(
                &json!({ "rowId": "task-1", "generation": 0, "pass": true }),
                &context,
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }
}
