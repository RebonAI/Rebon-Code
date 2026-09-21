//! `TodoWrite`: the pre-Task todo list.
//!
//! The store it writes to is `rebon_tool::todo_write`, not this plugin's: the
//! TUI's task pane reads it directly and cannot depend on this plugin being
//! loaded. What is this plugin's is the tool — the schema, the validation and
//! the state-replacement call.

use async_trait::async_trait;
use rebon_tool::todo_write::{get_todos, set_todos, TodoItem, DEFAULT_TODO_KEY};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};

pub const TODO_WRITE_TOOL_NAME: &str = "TodoWrite";
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn id(&self) -> ToolId {
        ToolId::new(TODO_WRITE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["TodoWriteTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Task
    }

    fn is_enabled(&self) -> bool {
        !rebon_tool::tasks::is_todo_v2_enabled()
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Use this tool to create and manage a structured task list for your current coding session. \
         This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.\n\
         \n\
         ## When to Use This Tool\n\
         Use this tool proactively in these scenarios:\n\
         1. Complex multi-step tasks - When a task requires 3 or more distinct steps or actions\n\
         2. Non-trivial and complex tasks - Tasks that require careful planning or multiple operations\n\
         3. User explicitly requests todo list - When the user directly asks you to use the todo list\n\
         4. User provides multiple tasks - When users provide a list of things to be done\n\
         5. After receiving new instructions - Immediately capture user requirements as todos\n\
         6. When you start working on a task - Mark it as in_progress BEFORE beginning work\n\
         7. After completing a task - Mark it as completed and add any new follow-up tasks\n\
         \n\
         ## When NOT to Use This Tool\n\
         Skip using this tool when:\n\
         1. There is only a single, straightforward task\n\
         2. The task is trivial and tracking it provides no organizational benefit\n\
         3. The task can be completed in less than 3 trivial steps\n\
         4. The task is purely conversational or informational\n\
         \n\
         ## Task States and Management\n\
         - pending: Task not yet started\n\
         - in_progress: Currently working on (limit to ONE task at a time)\n\
         - completed: Task finished successfully\n\
         \n\
         IMPORTANT: Task descriptions must have two forms:\n\
         - content: The imperative form (e.g., \"Run tests\")\n\
         - activeForm: The present continuous form (e.g., \"Running tests\")\n\
         \n\
         Mark tasks complete IMMEDIATELY after finishing (don't batch completions). \
         Exactly ONE task must be in_progress at any time. \
         ONLY mark a task as completed when you have FULLY accomplished it."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Imperative task description"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current task status"
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Present continuous form for spinner display"
                            }
                        },
                        "required": ["content", "status", "activeForm"],
                        "additionalProperties": false
                    },
                    "description": "The updated todo list"
                }
            },
            "required": ["todos"],
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

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        let parsed = parse_input(&input)?;

        // Resolve the todo key. It should be the agent id, falling back to
        // the session id; we use the default key since ToolContext doesn't
        // carry an agent id yet.
        let todo_key = DEFAULT_TODO_KEY;

        let old_todos = get_todos(todo_key);
        let all_done = parsed.todos.iter().all(|t| t.status == "completed");
        let new_todos = if all_done {
            Vec::new()
        } else {
            parsed.todos.clone()
        };

        set_todos(todo_key, new_todos);

        Ok(json!({
            "oldTodos": old_todos,
            "newTodos": parsed.todos,
        }))
    }
}

struct ParsedTodoWriteInput {
    todos: Vec<TodoItem>,
}

fn parse_input(input: &Value) -> ToolResult<ParsedTodoWriteInput> {
    let tool = ToolId::new(TODO_WRITE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "TodoWrite input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let todos_value = object.get("todos").ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "TodoWrite input requires a `todos` array".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let todos_array = todos_value
        .as_array()
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "`todos` must be an array".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

    let mut todos = Vec::with_capacity(todos_array.len());
    for (i, item) in todos_array.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("todos[{i}] must be an object"),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

        let content =
            obj.get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: format!("todos[{i}].content must be a non-empty string"),
                    error_code: Some(INVALID_INPUT_CODE),
                })?;
        if content.is_empty() {
            return Err(ToolError::InvalidInput {
                tool,
                reason: format!("todos[{i}].content cannot be empty"),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }

        let status =
            obj.get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: format!("todos[{i}].status must be a string"),
                    error_code: Some(INVALID_INPUT_CODE),
                })?;
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return Err(ToolError::InvalidInput {
                tool,
                reason: format!("todos[{i}].status must be pending, in_progress, or completed"),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }

        let active_form = obj
            .get("activeForm")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!("todos[{i}].activeForm must be a non-empty string"),
                error_code: Some(INVALID_INPUT_CODE),
            })?;
        if active_form.is_empty() {
            return Err(ToolError::InvalidInput {
                tool,
                reason: format!("todos[{i}].activeForm cannot be empty"),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }

        todos.push(TodoItem {
            content: content.to_string(),
            status: status.to_string(),
            active_form: active_form.to_string(),
        });
    }

    Ok(ParsedTodoWriteInput { todos })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::todo_write::clear_all_todos;
    use serde_json::json;

    fn tool() -> TodoWriteTool {
        TodoWriteTool
    }

    fn sample_todos() -> Value {
        json!({
            "todos": [
                {
                    "content": "Fix authentication bug",
                    "status": "in_progress",
                    "activeForm": "Fixing authentication bug"
                },
                {
                    "content": "Run tests",
                    "status": "pending",
                    "activeForm": "Running tests"
                }
            ]
        })
    }

    // ---------------------------------------------------------------
    // Input validation
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn validate_accepts_well_formed_input() {
        let result = tool()
            .validate_input(&sample_todos(), &ToolContext::new())
            .await
            .unwrap();
        assert!(result.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_missing_todos_key() {
        let result = tool()
            .validate_input(&json!({}), &ToolContext::new())
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_non_array_todos() {
        let result = tool()
            .validate_input(&json!({"todos": "not an array"}), &ToolContext::new())
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_empty_content() {
        let result = tool()
            .validate_input(
                &json!({"todos": [{"content": "", "status": "pending", "activeForm": "Doing"}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_invalid_status() {
        let result = tool()
            .validate_input(
                &json!({"todos": [{"content": "Task", "status": "done", "activeForm": "Doing"}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn validate_rejects_missing_active_form() {
        let result = tool()
            .validate_input(
                &json!({"todos": [{"content": "Task", "status": "pending"}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_valid());
    }

    #[tokio::test]
    async fn validate_accepts_empty_todos_array() {
        let result = tool()
            .validate_input(&json!({"todos": []}), &ToolContext::new())
            .await
            .unwrap();
        assert!(result.is_valid());
    }

    // ---------------------------------------------------------------
    // call — basic storage
    // ---------------------------------------------------------------

    // All state-dependent tests are grouped into a single test to avoid
    // global `TODOS` races when cargo runs tests in parallel.
    #[tokio::test]
    async fn call_lifecycle_stores_returns_and_clears() {
        clear_all_todos();

        // --- Step 1: first call stores todos and returns empty oldTodos ---
        let out = tool()
            .call(sample_todos(), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out["oldTodos"], json!([]));
        assert_eq!(out["newTodos"].as_array().unwrap().len(), 2);

        let stored = get_todos(DEFAULT_TODO_KEY);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].content, "Fix authentication bug");
        assert_eq!(stored[0].status, "in_progress");
        assert_eq!(stored[1].content, "Run tests");

        // --- Step 2: second call returns previous todos as oldTodos ---
        let out = tool()
            .call(
                json!({
                    "todos": [
                        {"content": "Fix authentication bug", "status": "completed", "activeForm": "Fixing authentication bug"},
                        {"content": "Run tests", "status": "in_progress", "activeForm": "Running tests"}
                    ]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["oldTodos"].as_array().unwrap().len(), 2);
        assert_eq!(out["newTodos"].as_array().unwrap().len(), 2);
        // Not all completed → list is stored
        assert_eq!(get_todos(DEFAULT_TODO_KEY).len(), 2);

        // --- Step 3: all-completed clears in-memory state ---
        let out = tool()
            .call(
                json!({
                    "todos": [
                        {"content": "Fix authentication bug", "status": "completed", "activeForm": "Fixing authentication bug"},
                        {"content": "Run tests", "status": "completed", "activeForm": "Running tests"}
                    ]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["newTodos"].as_array().unwrap().len(), 2);
        assert!(get_todos(DEFAULT_TODO_KEY).is_empty());

        // --- Step 4: empty array also clears ---
        tool()
            .call(sample_todos(), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(get_todos(DEFAULT_TODO_KEY).len(), 2);

        let out = tool()
            .call(json!({"todos": []}), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out["oldTodos"].as_array().unwrap().len(), 2);
        assert_eq!(out["newTodos"], json!([]));
        assert!(get_todos(DEFAULT_TODO_KEY).is_empty());
    }

    // ---------------------------------------------------------------
    // tool metadata
    // ---------------------------------------------------------------

    #[test]
    fn todo_write_tool_exposes_alias() {
        assert_eq!(tool().aliases(), &["TodoWriteTool"]);
    }

    #[test]
    fn todo_write_tool_is_concurrency_safe() {
        assert!(tool().is_concurrency_safe(&json!({})));
    }
}
