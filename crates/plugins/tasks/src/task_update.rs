use async_trait::async_trait;
use rebon_tool::tasks::{
    block_task, delete_task, get_task, update_task, TaskListStatus, TaskPatch,
};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Map, Value};

pub const TASK_UPDATE_TOOL_NAME: &str = "TaskUpdate";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct TaskUpdateTool;

#[derive(Debug, Clone)]
struct TaskUpdateInput {
    task_id: String,
    subject: Option<String>,
    description: Option<String>,
    active_form: Option<String>,
    status: Option<TaskUpdateStatus>,
    add_blocks: Option<Vec<String>>,
    add_blocked_by: Option<Vec<String>>,
    owner: Option<String>,
    metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskUpdateStatus {
    Deleted,
    Value(TaskListStatus),
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn id(&self) -> ToolId {
        ToolId::new(TASK_UPDATE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TaskUpdateTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Task
    }

    fn is_enabled(&self) -> bool {
        rebon_tool::tasks::is_todo_v2_enabled()
    }

    fn description(&self) -> &str {
        "Use this tool to update a task in the task list.\n\
         \n\
         ## When to Use This Tool\n\
         - Mark tasks as resolved when you have completed the work\n\
         - IMPORTANT: Always mark your assigned tasks as resolved when you finish them\n\
         - After resolving, call TaskList to find your next task\n\
         \n\
         ## Fields You Can Update\n\
         - status: pending, in_progress, completed, or deleted\n\
         - subject: Change the task title (imperative form)\n\
         - description: Change the task description\n\
         - activeForm: Present continuous form for spinner display\n\
         - owner: Change the task owner (agent name). Owner assignments only \
         stick when the task is or stays in_progress; changing status to \
         pending/completed/deleted in the same call clears the owner.\n\
         \n\
         ## Completion Requirements\n\
         - ONLY mark a task as completed when you have FULLY accomplished it\n\
         - If you encounter errors or blockers, keep the task as in_progress\n\
         - Never mark a task as completed if tests are failing or implementation is partial\n\
         \n\
         Status workflow: pending \u{2192} in_progress \u{2192} completed"
    }

    fn model_description(&self) -> &str {
        "Update an existing task's status or fields. Mark tasks completed only after fully accomplishing them."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "taskId": { "type": "string", "description": "The ID of the task to update" },
                "subject": { "type": "string", "description": "New subject for the task" },
                "description": { "type": "string", "description": "New description for the task" },
                "activeForm": {
                    "type": "string",
                    "description": "Present continuous form shown in spinner when in_progress (e.g., \"Running tests\")"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "deleted"],
                    "description": "New status for the task"
                },
                "addBlocks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task IDs that this task blocks"
                },
                "addBlockedBy": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task IDs that block this task"
                },
                "owner": {
                    "type": "string",
                    "description": "New owner (agent name). Owner assignment only sticks when the task is or stays in_progress; changing status to pending/completed/deleted in the same call clears the owner."
                },
                "metadata": {
                    "type": "object",
                    "description": "Metadata keys to merge into the task. Set a key to null to delete it.",
                    "additionalProperties": true
                }
            },
            "required": ["taskId"],
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
        let task_list_id = context.task_list_id();
        let Some(existing_task) =
            get_task(&task_list_id, &parsed.task_id).map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            })?
        else {
            return Ok(json!({
                "success": false,
                "taskId": parsed.task_id,
                "updatedFields": [],
                "error": "Task not found"
            }));
        };

        let mut updated_fields = Vec::new();
        let mut patch = TaskPatch::default();
        let mut status_change: Option<(String, String)> = None;

        if let Some(subject) = parsed.subject {
            if subject != existing_task.subject {
                patch.subject = Some(subject);
                updated_fields.push("subject".to_string());
            }
        }
        if let Some(description) = parsed.description {
            if description != existing_task.description {
                patch.description = Some(description);
                updated_fields.push("description".to_string());
            }
        }
        if let Some(active_form) = parsed.active_form {
            if Some(active_form.as_str()) != existing_task.active_form.as_deref() {
                patch.active_form = Some(Some(active_form));
                updated_fields.push("activeForm".to_string());
            }
        }
        // `update_task_unlocked` clears the owner whenever a status change to
        // anything other than in_progress is applied. Detect that here so
        // `updatedFields` doesn't claim an owner assignment the persisted task
        // immediately drops. A no-op status (`next == existing`) is not applied
        // as a patch, so it does not clear the owner.
        let status_clears_owner = matches!(
            &parsed.status,
            Some(TaskUpdateStatus::Value(next))
                if *next != existing_task.status && *next != TaskListStatus::InProgress
        );
        if let Some(owner) = parsed.owner {
            if Some(owner.as_str()) != existing_task.owner.as_deref() {
                patch.owner = Some(Some(owner));
                if !status_clears_owner {
                    updated_fields.push("owner".to_string());
                }
            }
        }
        if let Some(metadata) = parsed.metadata {
            let mut merged = existing_task.metadata.clone().unwrap_or_default();
            for (key, value) in metadata {
                if value.is_null() {
                    merged.remove(&key);
                } else {
                    merged.insert(key, value);
                }
            }
            patch.metadata = Some(Some(merged));
            updated_fields.push("metadata".to_string());
        }
        if let Some(status) = parsed.status {
            match status {
                TaskUpdateStatus::Deleted => {
                    let deleted = delete_task(&task_list_id, &parsed.task_id).map_err(|err| {
                        ToolError::Execution {
                            tool: self.id(),
                            source: err.into(),
                        }
                    })?;
                    return Ok(json!({
                        "success": deleted,
                        "taskId": parsed.task_id,
                        "updatedFields": if deleted { vec!["deleted"] } else { Vec::<&str>::new() },
                        "error": if deleted { Value::Null } else { json!("Failed to delete task") },
                        "statusChange": if deleted {
                            json!({
                                "from": existing_task.status.to_string(),
                                "to": "deleted"
                            })
                        } else {
                            Value::Null
                        }
                    }));
                }
                TaskUpdateStatus::Value(next_status) if next_status != existing_task.status => {
                    patch.status = Some(next_status.clone());
                    updated_fields.push("status".to_string());
                    status_change =
                        Some((existing_task.status.to_string(), next_status.to_string()));
                }
                TaskUpdateStatus::Value(_) => {}
            }
        }

        if patch.subject.is_some()
            || patch.description.is_some()
            || patch.active_form.is_some()
            || patch.owner.is_some()
            || patch.status.is_some()
            || patch.metadata.is_some()
        {
            update_task(&task_list_id, &parsed.task_id, patch).map_err(|err| {
                ToolError::Execution {
                    tool: self.id(),
                    source: err.into(),
                }
            })?;
        }

        if let Some(add_blocks) = parsed.add_blocks {
            let new_blocks: Vec<String> = add_blocks
                .into_iter()
                .filter(|id| !existing_task.blocks.iter().any(|existing| existing == id))
                .collect();
            for block_id in &new_blocks {
                let _ = block_task(&task_list_id, &parsed.task_id, block_id).map_err(|err| {
                    ToolError::Execution {
                        tool: self.id(),
                        source: err.into(),
                    }
                })?;
            }
            if !new_blocks.is_empty() {
                updated_fields.push("blocks".to_string());
            }
        }

        if let Some(add_blocked_by) = parsed.add_blocked_by {
            let new_blocked_by: Vec<String> = add_blocked_by
                .into_iter()
                .filter(|id| {
                    !existing_task
                        .blocked_by
                        .iter()
                        .any(|existing| existing == id)
                })
                .collect();
            for blocker_id in &new_blocked_by {
                let _ = block_task(&task_list_id, blocker_id, &parsed.task_id).map_err(|err| {
                    ToolError::Execution {
                        tool: self.id(),
                        source: err.into(),
                    }
                })?;
            }
            if !new_blocked_by.is_empty() {
                updated_fields.push("blockedBy".to_string());
            }
        }

        Ok(json!({
            "success": true,
            "taskId": parsed.task_id,
            "updatedFields": updated_fields,
            "statusChange": status_change.map(|(from, to)| json!({ "from": from, "to": to })),
        }))
    }
}

fn parse_input(input: &Value) -> ToolResult<TaskUpdateInput> {
    let tool = ToolId::new(TASK_UPDATE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "TaskUpdate input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    Ok(TaskUpdateInput {
        task_id: required_string(object.get("taskId"), "taskId", &tool)?,
        subject: optional_string(object.get("subject"), "subject", &tool)?,
        description: optional_string(object.get("description"), "description", &tool)?,
        active_form: optional_string(object.get("activeForm"), "activeForm", &tool)?,
        status: optional_status(object.get("status"), &tool)?,
        add_blocks: optional_string_array(object.get("addBlocks"), "addBlocks", &tool)?,
        add_blocked_by: optional_string_array(object.get("addBlockedBy"), "addBlockedBy", &tool)?,
        owner: optional_string(object.get("owner"), "owner", &tool)?,
        metadata: optional_object(object.get("metadata"), "metadata", &tool)?,
    })
}

fn required_string(value: Option<&Value>, field: &str, tool: &ToolId) -> ToolResult<String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("TaskUpdate input requires a string `{field}`"),
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

fn optional_string_array(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<Vec<String>>> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| ToolError::InvalidInput {
                        tool: tool.clone(),
                        reason: format!("`{field}` must be an array of strings when provided"),
                        error_code: Some(INVALID_INPUT_CODE),
                    })
            })
            .collect::<ToolResult<Vec<_>>>()
            .map(Some),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be an array when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

fn optional_status(value: Option<&Value>, tool: &ToolId) -> ToolResult<Option<TaskUpdateStatus>> {
    match value {
        Some(Value::String(raw)) => match raw.as_str() {
            "pending" => Ok(Some(TaskUpdateStatus::Value(TaskListStatus::Pending))),
            "in_progress" => Ok(Some(TaskUpdateStatus::Value(TaskListStatus::InProgress))),
            "completed" => Ok(Some(TaskUpdateStatus::Value(TaskListStatus::Completed))),
            "deleted" => Ok(Some(TaskUpdateStatus::Deleted)),
            other => Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!("Unsupported status `{other}`"),
                error_code: Some(INVALID_INPUT_CODE),
            }),
        },
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "`status` must be a string when provided".into(),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::tasks::{create_task, get_task, test_support::TestConfigHome, NewTask};
    use serde_json::json;

    fn tool() -> TaskUpdateTool {
        TaskUpdateTool
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
    async fn call_updates_fields_and_merges_metadata() {
        let home = TestConfigHome::new("task-update");
        let task_id = create_task(home.task_list_id(), sample_task("Ship feature")).unwrap();

        let out = tool()
            .call(
                json!({
                    "taskId": task_id,
                    "subject": "Ship feature v2",
                    "status": "in_progress",
                    "metadata": {
                        "priority": "high"
                    }
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["success"], json!(true));
        assert_eq!(
            out["updatedFields"],
            json!(["subject", "metadata", "status"])
        );
        assert_eq!(out["statusChange"]["from"], json!("pending"));
        assert_eq!(out["statusChange"]["to"], json!("in_progress"));

        let updated = get_task(home.task_list_id(), "1").unwrap().unwrap();
        assert_eq!(updated.subject, "Ship feature v2");
        assert_eq!(updated.status, TaskListStatus::InProgress);
        assert_eq!(updated.metadata.unwrap()["priority"], json!("high"));
    }

    #[tokio::test]
    async fn call_omits_owner_from_updated_fields_when_status_clears_it() {
        let home = TestConfigHome::new("task-update-owner-status");
        let task_id = create_task(
            home.task_list_id(),
            NewTask {
                owner: Some("agent-a".into()),
                status: TaskListStatus::InProgress,
                ..sample_task("owned")
            },
        )
        .unwrap();

        let out = tool()
            .call(
                json!({
                    "taskId": task_id,
                    "owner": "agent-b",
                    "status": "completed"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["success"], json!(true));
        let updated_fields = out["updatedFields"].as_array().unwrap();
        assert!(
            !updated_fields.iter().any(|field| field == "owner"),
            "owner is cleared by the status rule and must not be reported: {updated_fields:?}"
        );
        assert!(updated_fields.iter().any(|field| field == "status"));

        // The persisted task really has no owner — the response no longer lies.
        let persisted = get_task(home.task_list_id(), "1").unwrap().unwrap();
        assert_eq!(persisted.owner, None);
        assert_eq!(persisted.status, TaskListStatus::Completed);
    }

    #[tokio::test]
    async fn call_reports_owner_when_status_stays_in_progress() {
        let home = TestConfigHome::new("task-update-owner-inprogress");
        let task_id = create_task(home.task_list_id(), sample_task("assign")).unwrap();

        let out = tool()
            .call(
                json!({
                    "taskId": task_id,
                    "owner": "agent-b",
                    "status": "in_progress"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let updated_fields = out["updatedFields"].as_array().unwrap();
        assert!(
            updated_fields.iter().any(|field| field == "owner"),
            "owner assignment sticks with an in_progress status: {updated_fields:?}"
        );
        let persisted = get_task(home.task_list_id(), "1").unwrap().unwrap();
        assert_eq!(persisted.owner.as_deref(), Some("agent-b"));
    }

    #[tokio::test]
    async fn call_adds_block_relationships() {
        let home = TestConfigHome::new("task-update-blocks");
        let blocker = create_task(home.task_list_id(), sample_task("blocker")).unwrap();
        let blocked = create_task(home.task_list_id(), sample_task("blocked")).unwrap();

        let out = tool()
            .call(
                json!({
                    "taskId": blocker,
                    "addBlocks": [blocked]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["updatedFields"], json!(["blocks"]));
        let blocked_task = get_task(home.task_list_id(), "2").unwrap().unwrap();
        assert_eq!(blocked_task.blocked_by, vec!["1".to_string()]);
    }

    #[tokio::test]
    async fn call_deletes_task_when_status_is_deleted() {
        let home = TestConfigHome::new("task-update-delete");
        let task_id = create_task(home.task_list_id(), sample_task("cleanup")).unwrap();

        let out = tool()
            .call(
                json!({
                    "taskId": task_id,
                    "status": "deleted"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["success"], json!(true));
        assert_eq!(out["updatedFields"], json!(["deleted"]));
        assert_eq!(get_task(home.task_list_id(), "1").unwrap(), None);
    }

    #[tokio::test]
    async fn call_returns_error_for_missing_task() {
        let _home = TestConfigHome::new("task-update-missing");
        let out = tool()
            .call(json!({ "taskId": "999" }), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out["success"], json!(false));
        assert_eq!(out["error"], json!("Task not found"));
    }

    #[test]
    fn task_update_tool_exposes_alias() {
        assert_eq!(tool().aliases(), &["TaskUpdateTool"]);
    }
}
