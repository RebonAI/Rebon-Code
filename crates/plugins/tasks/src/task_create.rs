use async_trait::async_trait;
use rebon_tool::tasks::{create_task, NewTask, TaskListStatus};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Map, Value};

pub const TASK_CREATE_TOOL_NAME: &str = "TaskCreate";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct TaskCreateTool;

#[derive(Debug, Clone)]
struct TaskCreateInput {
    subject: String,
    description: String,
    active_form: Option<String>,
    metadata: Option<Map<String, Value>>,
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn id(&self) -> ToolId {
        ToolId::new(TASK_CREATE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TaskCreateTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Task
    }

    fn is_enabled(&self) -> bool {
        rebon_tool::tasks::is_todo_v2_enabled()
    }

    fn description(&self) -> &str {
        "Use this tool to create a structured task list for your current coding session. \
         This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.\n\
         \n\
         ## When to Use This Tool\n\
         Use this tool proactively in these scenarios:\n\
         - Complex multi-step tasks - When a task requires 3 or more distinct steps or actions\n\
         - Non-trivial and complex tasks - Tasks that require careful planning or multiple operations\n\
         - User explicitly requests todo list - When the user directly asks you to create tasks\n\
         - User provides multiple tasks - When users provide a list of things to be done\n\
         - After receiving new instructions - Immediately capture user requirements as tasks\n\
         \n\
         ## When NOT to Use This Tool\n\
         Skip using this tool when there is only a single, straightforward task, \
         the task is trivial, or the task is purely conversational.\n\
         \n\
         ## Task Fields\n\
         - subject: A brief, actionable title in imperative form (e.g., \"Fix authentication bug\")\n\
         - description: What needs to be done\n\
         - activeForm (optional): Present continuous form shown in the spinner (e.g., \"Fixing authentication bug\")\n\
         \n\
         All tasks are created with status `pending`."
    }

    fn model_description(&self) -> &str {
        "Create a structured task for tracking current-session coding work. Use for complex or multi-step tasks; created tasks start pending."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "A brief title for the task"
                },
                "description": {
                    "type": "string",
                    "description": "What needs to be done"
                },
                "activeForm": {
                    "type": "string",
                    "description": "Present continuous form shown in spinner when in_progress (e.g., \"Running tests\")"
                },
                "metadata": {
                    "type": "object",
                    "description": "Arbitrary metadata to attach to the task",
                    "additionalProperties": true
                }
            },
            "required": ["subject", "description"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
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
        let subject = parsed.subject.clone();
        let mut metadata = parsed.metadata;
        if let Some(agent_id) = context.agent_id() {
            let metadata = metadata.get_or_insert_with(Map::new);
            metadata
                .entry("agent_id".to_string())
                .or_insert_with(|| Value::String(agent_id.to_string()));
        }
        let task_id = create_task(
            &context.task_list_id(),
            NewTask {
                subject: parsed.subject,
                description: parsed.description,
                active_form: parsed.active_form,
                owner: None,
                status: TaskListStatus::Pending,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                metadata,
            },
        )
        .map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;

        Ok(json!({
            "task": {
                "id": task_id,
                "subject": subject,
            }
        }))
    }
}

fn parse_input(input: &Value) -> ToolResult<TaskCreateInput> {
    let tool = ToolId::new(TASK_CREATE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "TaskCreate input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    Ok(TaskCreateInput {
        subject: required_string(object.get("subject"), "subject", &tool)?,
        description: required_string(object.get("description"), "description", &tool)?,
        active_form: optional_string(object.get("activeForm"), "activeForm", &tool)?,
        metadata: optional_object(object.get("metadata"), "metadata", &tool)?,
    })
}

fn required_string(value: Option<&Value>, field: &str, tool: &ToolId) -> ToolResult<String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("TaskCreate input requires a string `{field}`"),
            error_code: Some(INVALID_INPUT_CODE),
        })
        .map(ToOwned::to_owned)
}

fn optional_string(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<String>> {
    match value {
        Some(Value::String(raw)) => Ok(Some(raw.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be a string when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

fn optional_object(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<Map<String, Value>>> {
    match value {
        Some(Value::Object(map)) => Ok(Some(map.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be an object when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::tasks::{get_task, test_support::TestConfigHome};
    use serde_json::json;

    fn tool() -> TaskCreateTool {
        TaskCreateTool
    }

    #[tokio::test]
    async fn call_creates_pending_task() {
        let home = TestConfigHome::new("task-create");
        let out = tool()
            .call(
                json!({
                    "subject": "Add tests",
                    "description": "Cover the new code path",
                    "activeForm": "Adding tests"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["task"]["id"], json!("1"));
        assert_eq!(out["task"]["subject"], json!("Add tests"));

        let task = get_task(home.task_list_id(), "1").unwrap().unwrap();
        assert_eq!(task.subject, "Add tests");
        assert_eq!(task.description, "Cover the new code path");
        assert_eq!(task.active_form.as_deref(), Some("Adding tests"));
        assert_eq!(task.status, TaskListStatus::Pending);
    }

    #[tokio::test]
    async fn call_tags_task_with_context_agent_id() {
        let home = TestConfigHome::new("task-create-agent-id");
        let out = tool()
            .call(
                json!({
                    "subject": "child task",
                    "description": "created by an agent"
                }),
                &ToolContext::new().with_agent_id("agent-a"),
            )
            .await
            .unwrap();
        let task_id = out["task"]["id"].as_str().unwrap();
        let task = get_task(home.task_list_id(), task_id).unwrap().unwrap();

        assert_eq!(task.metadata.unwrap()["agent_id"], "agent-a");
    }

    #[tokio::test]
    async fn call_preserves_explicit_task_agent_id_metadata() {
        let home = TestConfigHome::new("task-create-explicit-agent-id");
        let out = tool()
            .call(
                json!({
                    "subject": "child task",
                    "description": "created by an agent",
                    "metadata": { "agent_id": "explicit-agent" }
                }),
                &ToolContext::new().with_agent_id("agent-a"),
            )
            .await
            .unwrap();
        let task_id = out["task"]["id"].as_str().unwrap();
        let task = get_task(home.task_list_id(), task_id).unwrap().unwrap();

        assert_eq!(task.metadata.unwrap()["agent_id"], "explicit-agent");
    }

    #[tokio::test]
    async fn validate_input_rejects_non_object_metadata() {
        let result = tool()
            .validate_input(
                &json!({
                    "subject": "A",
                    "description": "B",
                    "metadata": "oops"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.error_code, Some(INVALID_INPUT_CODE));
        assert!(!result.is_valid());
    }

    #[test]
    fn task_create_tool_exposes_alias() {
        assert_eq!(tool().aliases(), &["TaskCreateTool"]);
    }
}
