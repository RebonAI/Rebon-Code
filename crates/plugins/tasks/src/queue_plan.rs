use async_trait::async_trait;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult};
use serde_json::{json, Value};

pub const QUEUE_PLAN_TOOL_NAME: &str = "QueuePlan";
const UNAVAILABLE_CODE: i64 = 400;

/// Read the Agent Queue this session coordinates.
///
/// A queue coordinator is woken one event at a time — a row finished, a review
/// came back — and each wake-up carries only that event. Without a way to see
/// the whole outline it has to decide from a keyhole: it cannot tell which rows
/// are still ahead, which are blocked on what, or which checkout a row would
/// continue from. This tool is that view.
#[derive(Debug, Clone, Default)]
pub struct QueuePlanTool;

#[async_trait]
impl Tool for QueuePlanTool {
    fn id(&self) -> ToolId {
        ToolId::new(QUEUE_PLAN_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Read the current state of the Agent Queue you are coordinating.\n\
         \n\
         Returns every task line with its status, dependencies, gates, review \
         verdicts, worktree and review baseline, plus the queue's concurrency \
         limit and how many lines are running.\n\
         \n\
         Call this whenever you need the whole picture rather than the single \
         event that woke you: before deciding what to dispatch next, when a \
         review comes back, or when judging whether a line is blocked.\n\
         \n\
         This is read-only. It reports what the queue is, not what it will do."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
        let Some(controller) = context.queue_controller() else {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "this session does not coordinate an Agent Queue".to_string(),
                error_code: Some(UNAVAILABLE_CODE),
            });
        };
        let Some(session_id) = context.session_id() else {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "this session has no id, so its queue cannot be resolved".to_string(),
                error_code: Some(UNAVAILABLE_CODE),
            });
        };
        match controller.queue_plan(session_id).await {
            Ok(Some(plan)) => Ok(plan),
            Ok(None) => Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: "this session is not attached to an Agent Queue".to_string(),
                error_code: Some(UNAVAILABLE_CODE),
            }),
            Err(error) => Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(error),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::QueueController;
    use std::sync::Arc;

    struct StubController(Result<Option<Value>, String>);

    #[async_trait]
    impl QueueController for StubController {
        async fn queue_plan(&self, _session_id: &str) -> Result<Option<Value>, String> {
            self.0.clone()
        }

        async fn submit_verdict(
            &self,
            _verdict: rebon_tool::QueueVerdict<'_>,
        ) -> Result<rebon_tool::QueueVerdictOutcome, String> {
            unreachable!("QueuePlan never submits a verdict")
        }

        async fn dispatch_row(
            &self,
            _session_id: &str,
            _row_id: &str,
            _worktree: &str,
        ) -> Result<rebon_tool::QueueVerdictOutcome, String> {
            unreachable!("QueuePlan never dispatches")
        }

        async fn block_row(
            &self,
            _session_id: &str,
            _row_id: &str,
            _reason: &str,
        ) -> Result<rebon_tool::QueueVerdictOutcome, String> {
            unreachable!("QueuePlan never blocks")
        }
    }

    fn context_for(controller: StubController) -> ToolContext {
        ToolContext::new()
            .with_session_id("sess-queue-1")
            .with_queue_controller(Arc::new(controller))
    }

    #[tokio::test]
    async fn returns_the_plan_from_the_controller() {
        let plan = json!({ "rows": [{ "id": "row-1", "status": "todo" }] });
        let context = context_for(StubController(Ok(Some(plan.clone()))));
        let out = QueuePlanTool.call(json!({}), &context).await.unwrap();
        assert_eq!(out, plan);
    }

    #[tokio::test]
    async fn a_session_without_a_queue_is_told_so_rather_than_given_an_empty_plan() {
        let out = QueuePlanTool.call(json!({}), &ToolContext::new()).await;
        assert!(out.is_err(), "expected an error, got {out:?}");

        let context = context_for(StubController(Ok(None)));
        let out = QueuePlanTool.call(json!({}), &context).await;
        assert!(out.is_err(), "expected an error, got {out:?}");
    }

    #[tokio::test]
    async fn controller_failures_surface_as_execution_errors() {
        let context = context_for(StubController(Err("store unreadable".into())));
        let error = QueuePlanTool
            .call(json!({}), &context)
            .await
            .expect_err("expected failure");
        assert!(format!("{error}").contains("store unreadable"));
    }
}
