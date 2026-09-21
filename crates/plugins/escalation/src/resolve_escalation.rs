use async_trait::async_trait;
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::str::FromStr;

use rebon_tool::{EscalationAnswer, EscalationId, EscalationSource, Tool, ToolContext};

pub const RESOLVE_ESCALATION_TOOL_NAME: &str = "ResolveEscalation";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct ResolveEscalationTool;

#[derive(Debug, Clone)]
struct ResolveEscalationInput {
    escalation_id: String,
    agent_id: String,
    answer: String,
    source: EscalationSource,
    instructions: Option<String>,
}

#[async_trait]
impl Tool for ResolveEscalationTool {
    fn id(&self) -> ToolId {
        ToolId::new(RESOLVE_ESCALATION_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Resolve a pending worker question escalation by escalation_id, sending an answer back to the blocked worker."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "escalation_id": { "type": "string", "description": "The escalation ID from the <question-escalation> message." },
                "agent_id": { "type": "string", "description": "The worker agent ID from the <question-escalation> message." },
                "answer": { "type": "string", "description": "The answer to send to the blocked worker." },
                "source": { "type": "string", "enum": ["user", "coordinator"], "description": "Whether the answer came directly from the user or coordinator judgment." },
                "instructions": { "type": "string", "description": "Optional follow-up instructions to the worker." }
            },
            "required": ["escalation_id", "agent_id", "answer", "source"],
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
        let resolver = context
            .escalation_resolver()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(
                    "ResolveEscalation requires a coordinator context with an escalation resolver"
                ),
            })?;
        let escalation_id = EscalationId::new(parsed.escalation_id);
        let answer_echo = parsed.answer.clone();
        let source_echo = parsed.source;
        let instructions_echo = parsed.instructions.clone();
        resolver
            .resolve(EscalationAnswer {
                escalation_id: escalation_id.clone(),
                agent_id: parsed.agent_id.clone(),
                answer: parsed.answer,
                source: source_echo,
                instructions: parsed.instructions,
            })
            .map_err(|reason| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(reason),
            })?;

        let mut result = serde_json::Map::new();
        result.insert("resolved".into(), json!(true));
        result.insert("answer".into(), json!(answer_echo));
        result.insert("source".into(), json!(source_echo.as_str()));
        if let Some(inst) = &instructions_echo {
            result.insert("instructions".into(), json!(inst));
        }
        Ok(Value::Object(result))
    }
}

fn parse_input(input: &Value) -> ToolResult<ResolveEscalationInput> {
    let tool = ToolId::new(RESOLVE_ESCALATION_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "ResolveEscalation input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let string_field = |name: &str| -> ToolResult<String> {
        object
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!("ResolveEscalation input requires a non-empty string `{name}`"),
                error_code: Some(INVALID_INPUT_CODE),
            })
    };

    let escalation_id = string_field("escalation_id")?;
    let agent_id = string_field("agent_id")?;
    let answer = string_field("answer")?;
    let source_raw = string_field("source")?;
    let source =
        EscalationSource::from_str(&source_raw).map_err(|reason| ToolError::InvalidInput {
            tool: tool.clone(),
            reason,
            error_code: Some(INVALID_INPUT_CODE),
        })?;
    let instructions = object
        .get("instructions")
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "ResolveEscalation `instructions` must be a string when provided"
                        .into(),
                    error_code: Some(INVALID_INPUT_CODE),
                })
        })
        .transpose()?;

    Ok(ResolveEscalationInput {
        escalation_id,
        agent_id,
        answer,
        source,
        instructions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::EscalationRegistry;

    #[tokio::test]
    async fn validates_source_enum() {
        let outcome = ResolveEscalationTool
            .validate_input(
                &json!({"escalation_id":"e","agent_id":"a","answer":"x","source":"bot"}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
    }

    #[tokio::test]
    async fn unknown_id_errors() {
        let registry = EscalationRegistry::new();
        let context = ToolContext::new().with_escalation_resolver(registry.resolver());
        let err = ResolveEscalationTool
            .call(json!({"escalation_id":"missing","agent_id":"a","answer":"x","source":"coordinator"}), &context)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown or already resolved"));
    }
}
