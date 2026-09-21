use async_trait::async_trait;
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};

use rebon_tool::{Tool, ToolContext};

pub const ESCALATE_QUESTION_TOOL_NAME: &str = "EscalateQuestion";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct EscalateQuestionTool;

#[derive(Debug, Clone)]
struct EscalateQuestionInput {
    question: String,
    options: Option<Vec<Value>>,
    context: Option<String>,
}

#[async_trait]
impl Tool for EscalateQuestionTool {
    fn id(&self) -> ToolId {
        ToolId::new(ESCALATE_QUESTION_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Escalate a blocking question from a worker to the coordinator and wait for an answer. Use for ambiguous requirements, competing approaches, risky operations, or conflicts that require coordinator/user clarification."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The non-empty question for the coordinator." },
                "options": {
                    "type": "array",
                    "description": "Optional suggested answers. Items may be simple strings or structured objects.",
                    "items": {}
                },
                "context": { "type": "string", "description": "Optional concise context/evidence for why the question blocks progress." }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_input(input))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input)?;
        let client = context.worker_escalation_client().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("EscalateQuestion is only available in worker contexts with an escalation client"),
        })?;
        let answer = client
            .escalate(parsed.question, parsed.options, parsed.context)
            .await
            .map_err(|reason| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(reason),
            })?;

        Ok(json!({
            "escalation_id": answer.escalation_id.as_str(),
            "answer": answer.answer,
            "source": answer.source.as_str(),
            "instructions": answer.instructions,
        }))
    }
}

fn parse_input(input: &Value) -> ToolResult<EscalateQuestionInput> {
    let tool = ToolId::new(ESCALATE_QUESTION_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "EscalateQuestion input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let question = object
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "EscalateQuestion input requires a non-empty string `question`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_string();

    let options = object
        .get("options")
        .map(|value| {
            value
                .as_array()
                .cloned()
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "EscalateQuestion `options` must be an array when provided".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                })
        })
        .transpose()?;

    let context = object
        .get("context")
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "EscalateQuestion `context` must be a string when provided".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                })
        })
        .transpose()?;

    Ok(EscalateQuestionInput {
        question,
        options,
        context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::{EscalationAnswer, EscalationRegistry, EscalationSource};

    #[tokio::test]
    async fn validates_non_empty_question() {
        let outcome = EscalateQuestionTool
            .validate_input(&json!({"question":"   "}), &ToolContext::new())
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn successful_send_answer_flow_without_permission() {
        let registry = EscalationRegistry::new();
        let client = registry.worker_client("agent-1", Some("Test agent".into()));
        let context = ToolContext::new().with_worker_escalation_client(client);
        let tool = EscalateQuestionTool;
        assert!(!tool.needs_permission(&json!({"question":"Q?"})));

        let handle = tokio::spawn({
            let context = context.clone();
            async move {
                EscalateQuestionTool
                    .call(
                        json!({"question":"Q?","options":["a", {"label":"b"}], "context":"ctx"}),
                        &context,
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        let note = registry.unnotified_question_notifications().pop().unwrap();
        registry
            .resolver()
            .resolve(EscalationAnswer {
                escalation_id: note.escalation_id.clone(),
                agent_id: "agent-1".into(),
                answer: "A".into(),
                source: EscalationSource::Coordinator,
                instructions: Some("continue".into()),
            })
            .unwrap();
        let result = handle.await.unwrap().unwrap();
        assert_eq!(result["escalation_id"], note.escalation_id.as_str());
        assert_eq!(result["answer"], "A");
        assert_eq!(result["source"], "coordinator");
        assert_eq!(result["instructions"], "continue");
    }

    #[tokio::test]
    async fn dropped_pending_path_returns_error() {
        let registry = EscalationRegistry::new();
        let client = registry.worker_client("agent-1", None);
        let context = ToolContext::new().with_worker_escalation_client(client);
        let handle = tokio::spawn({
            let context = context.clone();
            async move {
                EscalateQuestionTool
                    .call(json!({"question":"Q?"}), &context)
                    .await
            }
        });
        tokio::task::yield_now().await;
        registry.cancel_agent("agent-1", "stopped");
        let err = handle.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("cancelled before it was answered"));
    }
}
