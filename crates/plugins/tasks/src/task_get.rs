use async_trait::async_trait;
use rebon_tool::tasks::get_task;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};

pub const TASK_GET_TOOL_NAME: &str = "TaskGet";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct TaskGetTool;

#[derive(Debug, Clone)]
struct TaskGetInput {
    task_id: String,
}

#[async_trait]
impl Tool for TaskGetTool {
    fn id(&self) -> ToolId {
        ToolId::new(TASK_GET_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TaskGetTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Task
    }

    fn is_enabled(&self) -> bool {
        rebon_tool::tasks::is_todo_v2_enabled()
    }

    fn description(&self) -> &str {
        "Use this tool to retrieve a task by its ID from the task list.\n\
         \n\
         ## When to Use This Tool\n\
         - When you need the full description and context before starting work on a task\n\
         - To understand task dependencies (what it blocks, what blocks it)\n\
         - After being assigned a task, to get complete requirements\n\
         \n\
         ## Output\n\
         Returns full task details: subject, description, status, blocks, blockedBy.\n\
         \n\
         ## Tips\n\
         - After fetching a task, verify its blockedBy list is empty before beginning work.\n\
         - Use TaskList to see all tasks in summary form."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "taskId": {
                    "type": "string",
                    "description": "The ID of the task to retrieve"
                }
            },
            "required": ["taskId"],
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
        validation_outcome_from(parse_input(input))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input)?;
        let task = get_task(&context.task_list_id(), &parsed.task_id).map_err(|err| {
            ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            }
        })?;

        match task {
            Some(task) => Ok(json!({
                "task": {
                    "id": task.id,
                    "subject": task.subject,
                    "description": task.description,
                    "status": task.status,
                    "blocks": task.blocks,
                    "blockedBy": task.blocked_by,
                }
            })),
            None => Ok(json!({ "task": Value::Null })),
        }
    }
}

fn parse_input(input: &Value) -> ToolResult<TaskGetInput> {
    let tool = ToolId::new(TASK_GET_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "TaskGet input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let task_id = object
        .get("taskId")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            tool,
            reason: "TaskGet input requires a string `taskId`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_owned();

    Ok(TaskGetInput { task_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::tasks::{create_task, test_support::TestConfigHome, NewTask, TaskListStatus};
    use serde_json::json;

    fn tool() -> TaskGetTool {
        TaskGetTool
    }

    fn sample_task(subject: &str) -> NewTask {
        NewTask {
            subject: subject.into(),
            description: format!("{subject} description"),
            active_form: None,
            owner: None,
            status: TaskListStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: None,
        }
    }

    #[tokio::test]
    async fn call_returns_task_details() {
        let home = TestConfigHome::new("task-get");
        let task_id = create_task(home.task_list_id(), sample_task("Inspect logs")).unwrap();

        let out = tool()
            .call(json!({ "taskId": task_id }), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out["task"]["subject"], json!("Inspect logs"));
        assert_eq!(out["task"]["status"], json!("pending"));
        assert_eq!(out["task"]["blocks"], json!([]));
        assert_eq!(out["task"]["blockedBy"], json!([]));
    }

    #[tokio::test]
    async fn call_returns_null_when_task_is_missing() {
        let _home = TestConfigHome::new("task-get-missing");
        let out = tool()
            .call(json!({ "taskId": "999" }), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out["task"], Value::Null);
    }

    #[test]
    fn task_get_tool_exposes_alias() {
        assert_eq!(tool().aliases(), &["TaskGetTool"]);
    }
}
