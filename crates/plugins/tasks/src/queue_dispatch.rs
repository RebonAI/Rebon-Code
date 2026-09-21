//! `QueueDispatch` / `QueueBlock`: the two writes a queue coordinator makes
//! against one task line.
//!
//! `QueueContext` — the controller `ToolContext` carries — stayed in
//! `rebon_tool::queue`, because the context hands it out and the coordinator
//! sets it.

use async_trait::async_trait;
use rebon_tool::{QueueVerdictOutcome, Tool, ToolContext};
use rebon_tools_core::{
    parse_tool_input, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub const QUEUE_DISPATCH_TOOL_NAME: &str = "QueueDispatch";
pub const QUEUE_BLOCK_TOOL_NAME: &str = "QueueBlock";
const INVALID_INPUT_CODE: i64 = 400;
const MAX_REASON_CHARS: usize = 2_000;

/// Start one task line.
#[derive(Debug, Clone, Default)]
pub struct QueueDispatchTool;

/// Suspend one task line that cannot be judged.
#[derive(Debug, Clone, Default)]
pub struct QueueBlockTool;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RowInput {
    row_id: String,
    #[serde(default)]
    worktree: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BlockInput {
    row_id: String,
    reason: String,
}

/// Both tools resolve the same two things before they can act, and report a
/// refusal the same way.
fn controller_and_session<'a>(
    tool: &dyn Tool,
    context: &'a ToolContext,
) -> ToolResult<(&'a std::sync::Arc<dyn rebon_tool::QueueController>, &'a str)> {
    let Some(controller) = context.queue_controller() else {
        return Err(ToolError::InvalidInput {
            tool: tool.id(),
            reason: "this session does not coordinate an Agent Queue".to_string(),
            error_code: Some(INVALID_INPUT_CODE),
        });
    };
    let Some(session_id) = context.session_id() else {
        return Err(ToolError::InvalidInput {
            tool: tool.id(),
            reason: "this session has no id, so its queue cannot be resolved".to_string(),
            error_code: Some(INVALID_INPUT_CODE),
        });
    };
    Ok((controller, session_id))
}

/// A refusal is a normal answer, not a tool failure: the queue enforces rules
/// the coordinator cannot see the whole of, so being told "not yet, and why" is
/// information to act on rather than an error to retry.
fn outcome_value(row_id: &str, outcome: QueueVerdictOutcome, verb: &str) -> Value {
    match outcome {
        QueueVerdictOutcome::Applied { status } => json!({
            "applied": true,
            "rowId": row_id,
            "status": status,
        }),
        QueueVerdictOutcome::Rejected { reason } => json!({
            "applied": false,
            "rowId": row_id,
            "rejected": reason,
        }),
        QueueVerdictOutcome::Unconfirmed => json!({
            "applied": false,
            "rowId": row_id,
            "unconfirmed": format!(
                "the {verb} was submitted but the queue did not confirm in time; \
                 re-read QueuePlan before deciding anything else"
            ),
        }),
    }
}

#[async_trait]
impl Tool for QueueDispatchTool {
    fn id(&self) -> ToolId {
        ToolId::new(QUEUE_DISPATCH_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Start one task line of the Agent Queue you are coordinating.\n\
         \n\
         You choose what runs and in what order; the queue still enforces its \
         own rules. A row whose dependencies are unmet, that would exceed the \
         concurrency limit, or whose checkout another row is editing is \
         refused, and the refusal says which. Read `QueuePlan` first — \
         `dispatchable`, `blockedBy` and `freeSlots` tell you what will be \
         accepted.\n\
         \n\
         Dispatching is not waiting: the row starts and you are woken when it \
         finishes. Start every row you usefully can rather than one at a time.\n\
         \n\
         `worktree` decides one thing: whether this row can see the work that \
         came before it. The default continues a predecessor's checkout when \
         exactly one is available, which is right whenever the row builds on \
         that work; each row's `worktreePlan` in QueuePlan reports what the \
         default would do and what you can override it to.\n\
         \n\
         Pass `isolate` when the row must not see that work — it is another \
         attempt at something an earlier row already tried and the attempts \
         are meant to be independent, it is a check on earlier work that \
         seeing would bias, or you expect the two to collide. Pass `inherit` \
         when the row does build on earlier work and you would rather be told \
         that continuing is impossible than silently handed a clean tree. A \
         request that cannot be honoured is refused with the reason rather \
         than quietly turned into something else."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "rowId": {
                    "type": "string",
                    "description": "Row id from QueuePlan (the `id` field, not the line number)"
                },
                "worktree": {
                    "type": "string",
                    "enum": ["auto", "inherit", "isolate"],
                    "description": "Where the row runs. Omit for the queue's own policy."
                }
            },
            "required": ["rowId"],
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
        let parsed: RowInput = parse_tool_input(self.id(), input)?;
        if parsed.row_id.trim().is_empty() {
            return Ok(ValidationOutcome::invalid(
                "`rowId` is required".to_string(),
                INVALID_INPUT_CODE,
            ));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: RowInput = parse_tool_input(self.id(), &input)?;
        let (controller, session_id) = controller_and_session(self, context)?;
        let row_id = parsed.row_id.trim();
        let worktree = parsed.worktree.as_deref().unwrap_or("auto").trim();
        let outcome = controller
            .dispatch_row(session_id, row_id, worktree)
            .await
            .map_err(|error| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(error),
            })?;
        Ok(outcome_value(row_id, outcome, "dispatch"))
    }
}

#[async_trait]
impl Tool for QueueBlockTool {
    fn id(&self) -> ToolId {
        ToolId::new(QUEUE_BLOCK_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Suspend one task line you cannot decide about, and carry on with the \
         rest of the queue.\n\
         \n\
         Use this when finishing the line would need a judgement you are not in \
         a position to make — a large deletion, a refactor whose blast radius \
         you cannot see, a conflict between two lines' work. State what \
         decision is needed in `reason`; a human resolves it.\n\
         \n\
         A blocked line frees its concurrency slot but never counts as done: \
         lines that depend on it keep waiting, lines that do not carry on. If \
         downstream work should proceed without it, that is a human's call to \
         skip the line, not a side effect of blocking it.\n\
         \n\
         Prefer this over guessing. Blocking one line costs the queue one line; \
         a wrong guess can cost every line built on top of it."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "rowId": {
                    "type": "string",
                    "description": "Row id from QueuePlan (the `id` field, not the line number)"
                },
                "reason": {
                    "type": "string",
                    "description": "What decision is needed, and what you would need in order to make it"
                }
            },
            "required": ["rowId", "reason"],
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
        let parsed: BlockInput = parse_tool_input(self.id(), input)?;
        if parsed.row_id.trim().is_empty() {
            return Ok(ValidationOutcome::invalid(
                "`rowId` is required".to_string(),
                INVALID_INPUT_CODE,
            ));
        }
        // Blocking is what a human is handed; without a reason there is nothing
        // for them to resolve.
        if parsed.reason.trim().is_empty() {
            return Ok(ValidationOutcome::invalid(
                "`reason` is required: it is the decision a human has to make".to_string(),
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
        let parsed: BlockInput = parse_tool_input(self.id(), &input)?;
        let (controller, session_id) = controller_and_session(self, context)?;
        let row_id = parsed.row_id.trim();
        let outcome = controller
            .block_row(session_id, row_id, parsed.reason.trim())
            .await
            .map_err(|error| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(error),
            })?;
        Ok(outcome_value(row_id, outcome, "block"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::{QueueController, QueueVerdict};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct SpyController {
        dispatched: Mutex<Vec<String>>,
        blocked: Mutex<Vec<(String, String)>>,
        outcome: Option<QueueVerdictOutcome>,
    }

    #[async_trait]
    impl QueueController for SpyController {
        async fn queue_plan(&self, _session_id: &str) -> Result<Option<Value>, String> {
            Ok(None)
        }

        async fn submit_verdict(
            &self,
            _verdict: QueueVerdict<'_>,
        ) -> Result<QueueVerdictOutcome, String> {
            unreachable!()
        }

        async fn dispatch_row(
            &self,
            _session_id: &str,
            row_id: &str,
            worktree: &str,
        ) -> Result<QueueVerdictOutcome, String> {
            self.dispatched
                .lock()
                .unwrap()
                .push(format!("{row_id}:{worktree}"));
            Ok(self
                .outcome
                .clone()
                .unwrap_or(QueueVerdictOutcome::Applied {
                    status: "launching".into(),
                }))
        }

        async fn block_row(
            &self,
            _session_id: &str,
            row_id: &str,
            reason: &str,
        ) -> Result<QueueVerdictOutcome, String> {
            self.blocked
                .lock()
                .unwrap()
                .push((row_id.to_string(), reason.to_string()));
            Ok(QueueVerdictOutcome::Applied {
                status: "blocked".into(),
            })
        }
    }

    fn context_with(controller: Arc<SpyController>) -> ToolContext {
        ToolContext::new()
            .with_session_id("sess-coordinator-1")
            .with_queue_controller(controller)
    }

    #[tokio::test]
    async fn dispatch_reaches_the_writer() {
        let spy = Arc::new(SpyController::default());
        let out = QueueDispatchTool
            .call(json!({ "rowId": " task-7 " }), &context_with(spy.clone()))
            .await
            .unwrap();
        assert_eq!(out["applied"], json!(true));
        assert_eq!(spy.dispatched.lock().unwrap()[0], "task-7:auto");
    }

    #[tokio::test]
    async fn a_refused_dispatch_is_a_result_carrying_the_reason() {
        let spy = Arc::new(SpyController {
            outcome: Some(QueueVerdictOutcome::Rejected {
                reason: "blocked by task-3".into(),
            }),
            ..Default::default()
        });
        let out = QueueDispatchTool
            .call(json!({ "rowId": "task-7" }), &context_with(spy))
            .await
            .expect("a refusal is information, not a failure");
        assert_eq!(out["applied"], json!(false));
        assert_eq!(out["rejected"], json!("blocked by task-3"));
    }

    #[tokio::test]
    async fn blocking_requires_the_decision_that_is_needed() {
        let context = context_with(Arc::new(SpyController::default()));
        let outcome = QueueBlockTool
            .validate_input(&json!({ "rowId": "task-7", "reason": "  " }), &context)
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn blocking_carries_its_reason_through() {
        let spy = Arc::new(SpyController::default());
        QueueBlockTool
            .call(
                json!({ "rowId": "task-7", "reason": "needs a call on deleting the old module" }),
                &context_with(spy.clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            spy.blocked.lock().unwrap()[0],
            (
                "task-7".into(),
                "needs a call on deleting the old module".into()
            )
        );
    }
}
