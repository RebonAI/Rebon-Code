use async_trait::async_trait;
use rebon_tool::tasks::{get_task, update_task, TaskListStatus, TaskPatch};
use rebon_tool::{StopTaskOutcome, Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome};
use serde_json::{json, Value};

pub const TASK_STOP_TOOL_NAME: &str = "TaskStop";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn id(&self) -> ToolId {
        ToolId::new(TASK_STOP_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TaskStopTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Task
    }

    fn is_enabled(&self) -> bool {
        rebon_tool::tasks::is_todo_v2_enabled()
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Stops a running background task by its ID.\n\
         \n\
         - Takes a task_id parameter identifying the task to stop\n\
         - Returns a success or failure status\n\
         - Use this tool when you need to terminate a long-running task"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "taskId": {
                    "type": "string",
                    "description": "The ID of the task to stop"
                },
                "task_id": {
                    "type": "string",
                    "description": "Alias for taskId; provide exactly one of taskId or task_id"
                }
            },
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
        match task_id_from_input(input) {
            Some(_) => Ok(ValidationOutcome::valid()),
            None => Ok(ValidationOutcome::invalid(
                "TaskStop input requires a string `taskId` or `task_id`",
                INVALID_INPUT_CODE,
            )),
        }
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let task_id = task_id_from_input(&input).ok_or_else(|| ToolError::InvalidInput {
            tool: self.id(),
            reason: "TaskStop input requires a string `taskId` or `task_id`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

        if let Some(controller) = context.task_runtime_controller() {
            let session_id = context
                .session_id()
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: self.id(),
                    reason: "TaskStop runtime operations require a session_id".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                })?;
            match controller.stop_task(session_id, task_id).await {
                Ok(StopTaskOutcome::Stopped {
                    task_id,
                    task_type,
                    command,
                }) => {
                    return Ok(json!({
                        "success": true,
                        "taskId": task_id,
                        "task_id": task_id,
                        "mode": "runtime",
                        "taskType": task_type,
                        "task_type": task_type,
                        "command": command,
                    }));
                }
                Ok(StopTaskOutcome::NotFound) => {}
                Err(error) => {
                    return Ok(json!({
                        "success": false,
                        "taskId": task_id,
                        "task_id": task_id,
                        "mode": "runtime",
                        "error": error,
                    }));
                }
            }
        }

        stop_todo_task_fallback(self, context, task_id)
    }
}

fn task_id_from_input(input: &Value) -> Option<&str> {
    let object = input.as_object()?;
    object
        .get("taskId")
        .or_else(|| object.get("task_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn stop_todo_task_fallback(
    tool: &TaskStopTool,
    context: &ToolContext,
    task_id: &str,
) -> ToolResult<Value> {
    let task_list_id = context.task_list_id();
    let Some(existing) = get_task(&task_list_id, task_id).map_err(|err| ToolError::Execution {
        tool: tool.id(),
        source: err.into(),
    })?
    else {
        return Ok(json!({
            "success": false,
            "taskId": task_id,
            "task_id": task_id,
            "mode": "todo_fallback",
            "error": "Runtime task not found; TODO task not found"
        }));
    };

    if existing.status != TaskListStatus::InProgress {
        return Ok(json!({
            "success": false,
            "taskId": task_id,
            "task_id": task_id,
            "mode": "todo_fallback",
            "error": format!("Runtime task not found; TODO task is not in_progress (current status: {})", existing.status)
        }));
    }

    let patch = TaskPatch {
        status: Some(TaskListStatus::Pending),
        ..TaskPatch::default()
    };
    update_task(&task_list_id, task_id, patch).map_err(|err| ToolError::Execution {
        tool: tool.id(),
        source: err.into(),
    })?;

    Ok(json!({
        "success": true,
        "taskId": task_id,
        "task_id": task_id,
        "mode": "todo_fallback",
        "statusChange": {
            "from": "in_progress",
            "to": "pending"
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::tasks::{create_task, test_support::TestConfigHome, NewTask};
    use rebon_tool::TaskRuntimeController;
    use std::sync::{Arc, Mutex};

    struct MockController {
        outcome: Mutex<StopTaskOutcome>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TaskRuntimeController for MockController {
        async fn stop_task(
            &self,
            _session_id: &str,
            task_id: &str,
        ) -> Result<StopTaskOutcome, String> {
            self.calls
                .lock()
                .expect("mock calls poisoned")
                .push(task_id.to_string());
            Ok(self.outcome.lock().expect("mock outcome poisoned").clone())
        }

        async fn send_message_to_task(
            &self,
            _session_id: &str,
            _task_id: &str,
            _message: String,
        ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
            unreachable!()
        }
    }

    fn task(status: TaskListStatus) -> NewTask {
        NewTask {
            subject: "Work".into(),
            description: "Do work".into(),
            active_form: None,
            owner: None,
            status,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: None,
        }
    }

    #[tokio::test]
    async fn runtime_stop_success_uses_controller() {
        let controller = Arc::new(MockController {
            outcome: Mutex::new(StopTaskOutcome::Stopped {
                task_id: "agent-1".into(),
                task_type: "local_agent".into(),
                command: "Investigate".into(),
            }),
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("session-runtime")
            .with_task_runtime_controller(controller.clone() as Arc<dyn TaskRuntimeController>);
        let out = TaskStopTool
            .call(json!({"taskId": "agent-1"}), &context)
            .await
            .unwrap();

        assert_eq!(out["success"], json!(true));
        assert_eq!(out["mode"], json!("runtime"));
        assert_eq!(out["task_id"], json!("agent-1"));
        assert_eq!(
            controller.calls.lock().expect("mock calls poisoned")[0],
            "agent-1"
        );
    }

    #[tokio::test]
    async fn runtime_not_found_falls_back_to_todo_task() {
        let _home = TestConfigHome::new("task-stop-runtime-fallback");
        create_task(
            &rebon_tool::tasks::current_task_list_id(),
            task(TaskListStatus::InProgress),
        )
        .unwrap();
        let controller = Arc::new(MockController {
            outcome: Mutex::new(StopTaskOutcome::NotFound),
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("session-runtime")
            .with_task_runtime_controller(controller as Arc<dyn TaskRuntimeController>);
        let out = TaskStopTool
            .call(json!({"taskId": "1"}), &context)
            .await
            .unwrap();

        assert_eq!(out["success"], json!(true));
        assert_eq!(out["mode"], json!("todo_fallback"));
        assert_eq!(out["statusChange"]["to"], json!("pending"));
    }

    #[tokio::test]
    async fn accepts_task_id_alias() {
        let controller = Arc::new(MockController {
            outcome: Mutex::new(StopTaskOutcome::Stopped {
                task_id: "agent-alias".into(),
                task_type: "local_agent".into(),
                command: "Alias".into(),
            }),
            calls: Mutex::new(Vec::new()),
        });
        let context = ToolContext::new()
            .with_session_id("session-runtime")
            .with_task_runtime_controller(controller.clone() as Arc<dyn TaskRuntimeController>);
        let out = TaskStopTool
            .call(json!({"task_id": "agent-alias"}), &context)
            .await
            .unwrap();

        assert_eq!(out["success"], json!(true));
        assert_eq!(out["taskId"], json!("agent-alias"));
        assert_eq!(
            controller.calls.lock().expect("mock calls poisoned")[0],
            "agent-alias"
        );
    }
}
