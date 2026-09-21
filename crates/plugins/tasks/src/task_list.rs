use async_trait::async_trait;
use rebon_tool::tasks::{is_internal_task, list_tasks, TaskListStatus};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;

pub const TASK_LIST_TOOL_NAME: &str = "TaskList";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct TaskListTool;

#[derive(Debug, Serialize)]
struct TaskListEntry {
    id: String,
    subject: String,
    status: TaskListStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(rename = "blockedBy")]
    blocked_by: Vec<String>,
}

#[async_trait]
impl Tool for TaskListTool {
    fn id(&self) -> ToolId {
        ToolId::new(TASK_LIST_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TaskListTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Task
    }

    fn is_enabled(&self) -> bool {
        rebon_tool::tasks::is_todo_v2_enabled()
    }

    fn description(&self) -> &str {
        "Use this tool to list all tasks in the task list.\n\
         \n\
         ## When to Use This Tool\n\
         - To see what tasks are available to work on\n\
         - To check overall progress on the project\n\
         - To find tasks that are blocked and need dependencies resolved\n\
         - After completing a task, to check for newly unblocked work\n\
         - Prefer working on tasks in ID order (lowest ID first) when multiple tasks are available\n\
         \n\
         ## Output\n\
         Returns a summary of each task: id, subject, status, owner, blockedBy.\n\
         Use TaskGet with a specific task ID to view full details."
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

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match input {
            Value::Object(_) => Ok(ValidationOutcome::valid()),
            _ => Ok(ValidationOutcome::invalid(
                "TaskList input must be an object",
                INVALID_INPUT_CODE,
            )),
        }
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let validation = self.validate_input(&input, context).await?;
        if !validation.is_valid() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: validation
                    .message
                    .unwrap_or_else(|| "TaskList input is invalid".into()),
                error_code: validation.error_code,
            });
        }

        let tasks = list_tasks(&context.task_list_id()).map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;

        let visible_tasks: Vec<_> = tasks
            .into_iter()
            .filter(|task| !is_internal_task(task))
            .collect();
        let resolved_task_ids: HashSet<String> = visible_tasks
            .iter()
            .filter(|task| task.status == TaskListStatus::Completed)
            .map(|task| task.id.clone())
            .collect();

        let entries: Vec<TaskListEntry> = visible_tasks
            .into_iter()
            .map(|task| TaskListEntry {
                id: task.id,
                subject: task.subject,
                status: task.status,
                owner: task.owner,
                blocked_by: task
                    .blocked_by
                    .into_iter()
                    .filter(|id| !resolved_task_ids.contains(id))
                    .collect(),
            })
            .collect();

        Ok(json!({ "tasks": entries }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::tasks::{
        block_task, create_task, test_support::TestConfigHome, update_task, NewTask, TaskPatch,
    };
    use serde_json::{json, Map};

    fn tool() -> TaskListTool {
        TaskListTool
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
    async fn call_filters_internal_tasks() {
        let home = TestConfigHome::new("task-list-internal");
        let mut metadata = Map::new();
        metadata.insert("_internal".into(), json!(true));

        create_task(
            home.task_list_id(),
            NewTask {
                subject: "hidden".into(),
                description: "hidden description".into(),
                active_form: None,
                owner: None,
                status: TaskListStatus::Pending,
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                metadata: Some(metadata),
            },
        )
        .unwrap();
        create_task(home.task_list_id(), sample_task("visible")).unwrap();

        let out = tool().call(json!({}), &ToolContext::new()).await.unwrap();
        assert_eq!(out["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(out["tasks"][0]["subject"], json!("visible"));
    }

    #[tokio::test]
    async fn call_filters_completed_blockers_from_blocked_by() {
        let home = TestConfigHome::new("task-list-blocked-by");
        let blocker = create_task(home.task_list_id(), sample_task("blocker")).unwrap();
        let blocked = create_task(home.task_list_id(), sample_task("blocked")).unwrap();
        block_task(home.task_list_id(), &blocker, &blocked).unwrap();
        update_task(
            home.task_list_id(),
            &blocker,
            TaskPatch {
                status: Some(TaskListStatus::Completed),
                ..TaskPatch::default()
            },
        )
        .unwrap();

        let out = tool().call(json!({}), &ToolContext::new()).await.unwrap();
        let blocked_entry = out["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == json!(blocked))
            .unwrap();
        assert_eq!(blocked_entry["blockedBy"], json!([]));
    }

    #[test]
    fn task_list_tool_exposes_alias() {
        assert_eq!(tool().aliases(), &["TaskListTool"]);
    }
}
