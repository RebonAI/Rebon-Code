//! The router's tools: plan and cancel tasks, start, continue and stop
//! workers, hand threads to them, and look things up.
//!
//! Each tool resolves the [`WorkerControl`] its session's surface attached
//! and calls it. Only a router session sees these tools: every other session
//! filter denies them.

use std::sync::Arc;

use async_trait::async_trait;
use rebon_tool::{PlannedTask, Tool, ToolContext, WorkerControl, WorkerDispatch};
use rebon_tools_core::{
    parse_tool_input, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub const DISPATCH_WORKER_TOOL_NAME: &str = "DispatchWorker";
pub const CONTINUE_WORKER_TOOL_NAME: &str = "ContinueWorker";
pub const STOP_WORKER_TOOL_NAME: &str = "StopWorker";
pub const SUBSCRIBE_THREAD_TOOL_NAME: &str = "SubscribeThread";
pub const RECALL_TOOL_NAME: &str = "Recall";
pub const PLAN_TASKS_TOOL_NAME: &str = "PlanTasks";
pub const CANCEL_TASK_TOOL_NAME: &str = "CancelTask";

/// Every router tool name.
pub const ROUTER_TOOL_NAMES: &[&str] = &[
    PLAN_TASKS_TOOL_NAME,
    CANCEL_TASK_TOOL_NAME,
    DISPATCH_WORKER_TOOL_NAME,
    CONTINUE_WORKER_TOOL_NAME,
    STOP_WORKER_TOOL_NAME,
    SUBSCRIBE_THREAD_TOOL_NAME,
    RECALL_TOOL_NAME,
];

const INVALID_INPUT_CODE: i64 = 400;
const MAX_BRIEF_CHARS: usize = 40_000;

#[derive(Debug, Clone, Default)]
pub struct DispatchWorkerTool;
#[derive(Debug, Clone, Default)]
pub struct ContinueWorkerTool;
#[derive(Debug, Clone, Default)]
pub struct StopWorkerTool;
#[derive(Debug, Clone, Default)]
pub struct SubscribeThreadTool;
#[derive(Debug, Clone, Default)]
pub struct RecallTool;
#[derive(Debug, Clone, Default)]
pub struct PlanTasksTool;
#[derive(Debug, Clone, Default)]
pub struct CancelTaskTool;

/// The control and the router's session id, or the refusal every tool gives
/// outside a router session.
fn control_and_session<'a>(
    tool: &dyn Tool,
    context: &'a ToolContext,
) -> ToolResult<(&'a Arc<dyn WorkerControl>, &'a str)> {
    let refuse = |reason: &str| ToolError::InvalidInput {
        tool: tool.id(),
        reason: reason.to_string(),
        error_code: Some(INVALID_INPUT_CODE),
    };
    let control = context
        .worker_control()
        .ok_or_else(|| refuse("this session is not the router, so it has no workers"))?;
    let session_id = context
        .session_id()
        .ok_or_else(|| refuse("this session has no id, so its workers cannot be found"))?;
    Ok((control, session_id))
}

fn execution_error(tool: &dyn Tool, error: String) -> ToolError {
    ToolError::Execution {
        tool: tool.id(),
        source: anyhow::anyhow!(error),
    }
}

fn required(value: &str, field: &str) -> Option<ValidationOutcome> {
    value
        .trim()
        .is_empty()
        .then(|| ValidationOutcome::invalid(format!("`{field}` is required"), INVALID_INPUT_CODE))
}

/// An optional text field, trimmed; blank is the same as absent.
fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DispatchInput {
    project: String,
    title: String,
    brief: String,
    #[serde(default)]
    done_when: Option<String>,
    #[serde(default)]
    isolate: bool,
    #[serde(default)]
    model_profile: Option<String>,
    #[serde(default)]
    worker: Option<String>,
}

#[async_trait]
impl Tool for DispatchWorkerTool {
    fn id(&self) -> ToolId {
        ToolId::new(DISPATCH_WORKER_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Start a worker on one task in one project now. Returns the worker's id \
         and its task's id at once; you hear from it in a later digest, so do \
         not wait. For work in several steps, or that has to wait for other \
         work, use PlanTasks instead.\n\
         \n\
         The worker sees only `brief`. Make it self-contained: the goal, what \
         done looks like, what is already known (with ids it can look up), and \
         anything it must not do.\n\
         \n\
         Set `isolate` for a worker that will change code while other work may \
         touch the same checkout; it then works in its own git worktree. Leave \
         it off for research and read-only work. Leave `modelProfile` out \
         unless the task clearly needs a lighter (`fast`) or stronger \
         (`reasoning`) model than the default.\n\
         \n\
         Set `worker` to one of your workers to give it this task as its next \
         turn instead of starting a new one, when its session already knows \
         the ground: follow-up work in the same project or worktree. The task \
         waits while that worker is busy, and is checked like any other. A \
         worker idle past its prompt cache has its history compacted as it \
         resumes; one whose context has grown long is replaced by a fresh \
         worker on its own."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Absolute path of the project directory the worker works in"
                },
                "title": {
                    "type": "string",
                    "description": "A few words naming the task"
                },
                "brief": {
                    "type": "string",
                    "description": "Everything the worker needs to know"
                },
                "doneWhen": {
                    "type": "string",
                    "description": "What must hold for the task to be done, checkably"
                },
                "isolate": {
                    "type": "boolean",
                    "description": "Work in a git worktree of its own"
                },
                "modelProfile": {
                    "type": "string",
                    "description": "A model profile such as fast or reasoning; omit for the default"
                },
                "worker": {
                    "type": "string",
                    "description": "Id of one of your workers to take the task as its next turn; omit for a new worker"
                }
            },
            "required": ["project", "title", "brief"],
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
        let parsed: DispatchInput = parse_tool_input(self.id(), input)?;
        if let Some(invalid) = required(&parsed.project, "project")
            .or_else(|| required(&parsed.title, "title"))
            .or_else(|| required(&parsed.brief, "brief"))
        {
            return Ok(invalid);
        }
        if parsed.brief.chars().count() > MAX_BRIEF_CHARS {
            return Ok(ValidationOutcome::invalid(
                format!("`brief` is longer than {MAX_BRIEF_CHARS} characters; give ids to Recall instead of pasting everything"),
                INVALID_INPUT_CODE,
            ));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: DispatchInput = parse_tool_input(self.id(), &input)?;
        let (control, session_id) = control_and_session(self, context)?;
        let worker = trimmed(parsed.worker);
        let started = worker.is_none();
        let dispatched = control
            .dispatch(
                session_id,
                WorkerDispatch {
                    project: parsed.project.trim().to_string(),
                    title: parsed.title.trim().to_string(),
                    brief: parsed.brief,
                    done_when: parsed.done_when.unwrap_or_default().trim().to_string(),
                    isolate: parsed.isolate,
                    model_profile: trimmed(parsed.model_profile),
                    worker,
                },
            )
            .await
            .map_err(|error| execution_error(self, error))?;
        let mut result = json!({
            "workerId": dispatched.worker_id,
            "taskId": dispatched.task_id,
            "started": started,
        });
        if let Some(note) = dispatched.note {
            result["note"] = json!(note);
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PlannedTaskInput {
    key: String,
    title: String,
    project: String,
    goal: String,
    done_when: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    isolate: bool,
    #[serde(default)]
    model_profile: Option<String>,
    #[serde(default)]
    worker: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PlanInput {
    tasks: Vec<PlannedTaskInput>,
}

#[async_trait]
impl Tool for PlanTasksTool {
    fn id(&self) -> ToolId {
        ToolId::new(PLAN_TASKS_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Plan work as tasks. Each task gets a worker once the tasks it \
         depends on are done; the supervisor dispatches and follows them, so you \
         do not start or wait for them yourself. Returns each task's id at once.\n\
         \n\
         Give every task a `key` for this plan, and name the tasks it waits for in \
         `dependsOn`, by key or by the id of a task planned before. `goal` is the \
         worker's whole brief: it sees nothing else. `doneWhen` says, checkably, \
         what must hold for the task to be done. Tasks that do not depend on each \
         other run side by side; set `isolate` on a task that changes code while \
         others may touch the same checkout.\n\
         \n\
         A task that depends on one other task alone, on the same ground, goes \
         to that task's worker as its next turn, so the worker keeps what it \
         learned. Set `worker` to give a task to one of your workers by name: \
         follow-up work on ground it already knows. A worker idle past its \
         prompt cache has its history compacted as it resumes; one whose \
         context has grown long is replaced by a fresh worker on its own."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "This task's name within the plan" },
                            "title": { "type": "string", "description": "A few words naming the task" },
                            "project": { "type": "string", "description": "Absolute path of the project directory" },
                            "goal": { "type": "string", "description": "Everything the worker needs to know" },
                            "doneWhen": { "type": "string", "description": "What must hold for the task to be done, checkably" },
                            "dependsOn": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Keys in this plan, or ids of earlier tasks, that must be done first"
                            },
                            "isolate": { "type": "boolean", "description": "Work in a git worktree of its own" },
                            "modelProfile": { "type": "string", "description": "A model profile such as fast or reasoning; omit for the default" },
                            "worker": { "type": "string", "description": "Id of one of your workers to take it as its next turn; omit to let the supervisor choose" }
                        },
                        "required": ["key", "title", "project", "goal", "doneWhen"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tasks"],
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
        let parsed: PlanInput = parse_tool_input(self.id(), input)?;
        if parsed.tasks.is_empty() {
            return Ok(ValidationOutcome::invalid(
                "a plan needs at least one task".to_string(),
                INVALID_INPUT_CODE,
            ));
        }
        for task in &parsed.tasks {
            if let Some(invalid) = required(&task.key, "key")
                .or_else(|| required(&task.title, "title"))
                .or_else(|| required(&task.project, "project"))
                .or_else(|| required(&task.goal, "goal"))
                .or_else(|| required(&task.done_when, "doneWhen"))
            {
                return Ok(invalid);
            }
            if task.goal.chars().count() > MAX_BRIEF_CHARS {
                return Ok(ValidationOutcome::invalid(
                    format!(
                        "the goal of `{}` is longer than {MAX_BRIEF_CHARS} characters",
                        task.key
                    ),
                    INVALID_INPUT_CODE,
                ));
            }
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: PlanInput = parse_tool_input(self.id(), &input)?;
        let (control, session_id) = control_and_session(self, context)?;
        let keys: Vec<String> = parsed
            .tasks
            .iter()
            .map(|task| task.key.trim().to_string())
            .collect();
        let tasks = parsed
            .tasks
            .into_iter()
            .map(|task| PlannedTask {
                key: task.key.trim().to_string(),
                title: task.title.trim().to_string(),
                project: task.project.trim().to_string(),
                goal: task.goal,
                done_when: task.done_when.trim().to_string(),
                depends_on: task
                    .depends_on
                    .into_iter()
                    .map(|dep| dep.trim().to_string())
                    .filter(|dep| !dep.is_empty())
                    .collect(),
                isolate: task.isolate,
                model_profile: trimmed(task.model_profile),
                worker: trimmed(task.worker),
            })
            .collect();
        let ids = control
            .plan_tasks(session_id, tasks)
            .await
            .map_err(|error| execution_error(self, error))?;
        let tasks: serde_json::Map<String, Value> = keys
            .into_iter()
            .zip(ids)
            .map(|(key, id)| (key, Value::String(id)))
            .collect();
        Ok(json!({ "planned": true, "tasks": tasks }))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CancelInput {
    task_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[async_trait]
impl Tool for CancelTaskTool {
    fn id(&self) -> ToolId {
        ToolId::new(CANCEL_TASK_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Cancel one of your tasks: its worker is stopped and the task is not \
         dispatched again. Tasks that depend on it are blocked until you plan \
         them anew."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "taskId": { "type": "string", "description": "The task's id" },
                "reason": { "type": "string", "description": "Why, kept with the task" }
            },
            "required": ["taskId"],
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
        let parsed: CancelInput = parse_tool_input(self.id(), input)?;
        Ok(required(&parsed.task_id, "taskId").unwrap_or_else(ValidationOutcome::valid))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: CancelInput = parse_tool_input(self.id(), &input)?;
        let (control, session_id) = control_and_session(self, context)?;
        let task_id = parsed.task_id.trim();
        control
            .cancel_task(session_id, task_id, parsed.reason.as_deref())
            .await
            .map_err(|error| execution_error(self, error))?;
        Ok(json!({ "taskId": task_id, "cancelled": true }))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ContinueInput {
    worker_id: String,
    message: String,
    question_id: Option<u64>,
}

#[async_trait]
impl Tool for ContinueWorkerTool {
    fn id(&self) -> ToolId {
        ToolId::new(CONTINUE_WORKER_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Give one of your workers its next turn. It resumes its own session \
         with everything it learned so far, so prefer this to starting a fresh \
         worker on related work. Returns at once; a running worker takes the \
         message when its current turn ends. If Recall shows a pending auto-mode \
         question, pass its questionId with your decision to release that call \
         instead of queuing behind it. This withdraws only the question: your \
         guidance is not user authorization and grants no tool permissions. \
         Use it for an answer, a correction or a nudge. New work that should be \
         checked when it is done is a task: \
         give it to the worker with `worker` on DispatchWorker or PlanTasks."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "workerId": { "type": "string", "description": "The worker's id" },
                "message": { "type": "string", "description": "What to do next, or the router's decision on a pending question" },
                "questionId": { "type": "integer", "minimum": 0, "description": "Pending auto-mode question id from Recall; releases only that question, never a tool permission" }
            },
            "required": ["workerId", "message"],
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
        let parsed: ContinueInput = parse_tool_input(self.id(), input)?;
        Ok(required(&parsed.worker_id, "workerId")
            .or_else(|| required(&parsed.message, "message"))
            .unwrap_or_else(ValidationOutcome::valid))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: ContinueInput = parse_tool_input(self.id(), &input)?;
        let (control, session_id) = control_and_session(self, context)?;
        let worker_id = parsed.worker_id.trim();
        control
            .continue_worker(session_id, worker_id, &parsed.message, parsed.question_id)
            .await
            .map_err(|error| execution_error(self, error))?;
        Ok(
            json!({ "workerId": worker_id, "queued": parsed.question_id.is_none(),
            "questionResolved": parsed.question_id.is_some() }),
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkerInput {
    worker_id: String,
}

#[async_trait]
impl Tool for StopWorkerTool {
    fn id(&self) -> ToolId {
        ToolId::new(STOP_WORKER_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Stop one of your workers, and anything it started. Use it for a worker \
         going the wrong way, looping, or no longer needed."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "workerId": { "type": "string", "description": "The worker's id" }
            },
            "required": ["workerId"],
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
        let parsed: WorkerInput = parse_tool_input(self.id(), input)?;
        Ok(required(&parsed.worker_id, "workerId").unwrap_or_else(ValidationOutcome::valid))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: WorkerInput = parse_tool_input(self.id(), &input)?;
        let (control, session_id) = control_and_session(self, context)?;
        let worker_id = parsed.worker_id.trim();
        control
            .stop_worker(session_id, worker_id)
            .await
            .map_err(|error| execution_error(self, error))?;
        Ok(json!({ "workerId": worker_id, "stopped": true }))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SubscribeInput {
    thread: String,
    worker_id: String,
}

#[async_trait]
impl Tool for SubscribeThreadTool {
    fn id(&self) -> ToolId {
        ToolId::new(SUBSCRIBE_THREAD_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Hand a thread to a worker: later events on it go straight to that \
         worker without passing through you. A thread is a pull request \
         (`pr:owner/repo#12` or its URL: checks failing, changes requested, \
         merged, closed) or anything an outside source names `scheme:name`, \
         such as a chat thread `im:team#42` or a mail thread `mail:inbox/7`. \
         Handing a thread to a new worker takes it from the old one."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "thread": {
                    "type": "string",
                    "description": "pr:owner/repo#12, a pull request's URL, or scheme:name such as im:team#42"
                },
                "workerId": { "type": "string", "description": "The worker that owns it from now on" }
            },
            "required": ["thread", "workerId"],
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
        let parsed: SubscribeInput = parse_tool_input(self.id(), input)?;
        Ok(required(&parsed.thread, "thread")
            .or_else(|| required(&parsed.worker_id, "workerId"))
            .unwrap_or_else(ValidationOutcome::valid))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: SubscribeInput = parse_tool_input(self.id(), &input)?;
        let (control, session_id) = control_and_session(self, context)?;
        let worker_id = parsed.worker_id.trim();
        let thread = control
            .subscribe_thread(session_id, parsed.thread.trim(), worker_id)
            .await
            .map_err(|error| execution_error(self, error))?;
        Ok(json!({ "thread": thread, "workerId": worker_id, "subscribed": true }))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RecallInput {
    #[serde(default)]
    id: Option<String>,
}

#[async_trait]
impl Tool for RecallTool {
    fn id(&self) -> ToolId {
        ToolId::new(RECALL_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Look up an id: a task (`t-…`), a worker, a thread (`pr:owner/repo#12`) \
         or the router itself. You get a short digest with the ids next to it, which you can \
         Recall in turn: a worker lists its threads and last reply, a thread \
         names its worker, a task names its worker and what it waits for. With \
         no id, lists your open tasks and your workers. Follow the chain \
         instead of answering from memory."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "What to look up; omit to list your workers" }
            },
            "additionalProperties": false
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed: RecallInput = parse_tool_input(self.id(), &input)?;
        let (control, session_id) = control_and_session(self, context)?;
        let id = parsed
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let digest = control
            .recall(session_id, id)
            .await
            .map_err(|error| execution_error(self, error))?;
        Ok(Value::String(digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::DispatchedWorker;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
        /// The `worker` each dispatch and planned task asked for.
        workers: Mutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl WorkerControl for Recorder {
        async fn plan_tasks(
            &self,
            router: &str,
            tasks: Vec<PlannedTask>,
        ) -> Result<Vec<String>, String> {
            self.workers
                .lock()
                .unwrap()
                .extend(tasks.iter().map(|task| task.worker.clone()));
            self.calls.lock().unwrap().push(format!(
                "plan {router} {}",
                tasks
                    .iter()
                    .map(|task| format!("{}<-{}", task.key, task.depends_on.join("+")))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(tasks.iter().map(|task| format!("t-{}", task.key)).collect())
        }
        async fn cancel_task(
            &self,
            _router: &str,
            task: &str,
            _reason: Option<&str>,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("cancel {task}"));
            Ok(())
        }
        async fn dispatch(
            &self,
            router: &str,
            request: WorkerDispatch,
        ) -> Result<DispatchedWorker, String> {
            self.workers.lock().unwrap().push(request.worker.clone());
            self.calls.lock().unwrap().push(format!(
                "dispatch {router} {} {} isolate={}",
                request.project, request.title, request.isolate
            ));
            Ok(DispatchedWorker {
                worker_id: "w1".into(),
                task_id: Some("t-1".into()),
                note: None,
            })
        }
        async fn continue_worker(
            &self,
            router: &str,
            worker: &str,
            message: &str,
            question_id: Option<u64>,
        ) -> Result<(), String> {
            let question = question_id
                .map(|id| format!(" question={id}"))
                .unwrap_or_default();
            self.calls
                .lock()
                .unwrap()
                .push(format!("continue {router} {worker} {message}{question}"));
            Ok(())
        }
        async fn stop_worker(&self, _router: &str, worker: &str) -> Result<(), String> {
            Err(format!("{worker} is not one of your workers"))
        }
        async fn subscribe_thread(
            &self,
            _router: &str,
            thread: &str,
            _worker: &str,
        ) -> Result<String, String> {
            Ok(thread.to_lowercase())
        }
        async fn recall(&self, _router: &str, id: Option<&str>) -> Result<String, String> {
            Ok(format!("recall {}", id.unwrap_or("workers")))
        }
    }

    fn context(control: Arc<Recorder>) -> ToolContext {
        ToolContext::new()
            .with_session_id("router-session")
            .with_worker_control(control)
    }

    #[tokio::test]
    async fn the_tools_call_the_control_for_the_router_session() {
        let control = Arc::new(Recorder::default());
        let ctx = context(control.clone());
        let started = DispatchWorkerTool
            .call(
                json!({"project": "/p", "title": "Fix CI", "brief": "do it", "isolate": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(started["workerId"], "w1");
        assert_eq!(started["taskId"], "t-1");
        ContinueWorkerTool
            .call(json!({"workerId": " w1 ", "message": "next"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            *control.calls.lock().unwrap(),
            [
                "dispatch router-session /p Fix CI isolate=true",
                "continue router-session w1 next"
            ]
        );
        let thread = SubscribeThreadTool
            .call(json!({"thread": "PR:Acme/X#1", "workerId": "w1"}), &ctx)
            .await
            .unwrap();
        assert_eq!(thread["thread"], "pr:acme/x#1");
        let recalled = RecallTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(recalled, "recall workers");
        assert!(StopWorkerTool
            .call(json!({"workerId": "someone-else"}), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_router_question_reply_reaches_the_control_instead_of_the_next_turn_queue() {
        let control = Arc::new(Recorder::default());
        let ctx = context(control.clone());
        let result = ContinueWorkerTool
            .call(
                json!({
                    "workerId": "w1", "message": "Choose the compatible patch", "questionId": 42
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["queued"], false);
        assert_eq!(result["questionResolved"], true);
        assert_eq!(
            control.calls.lock().unwrap().last().unwrap(),
            "continue router-session w1 Choose the compatible patch question=42"
        );
        assert!(ContinueWorkerTool
            .validate_input(
                &json!({
                    "workerId": "w1", "message": "go", "questionId": -1
                }),
                &ctx
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_task_can_name_the_worker_to_take_it() {
        let control = Arc::new(Recorder::default());
        let ctx = context(control.clone());
        let queued = DispatchWorkerTool
            .call(
                json!({"project": "/p", "title": "Next", "brief": "do it", "worker": " w1 "}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(queued["started"], false);
        let started = DispatchWorkerTool
            .call(
                json!({"project": "/p", "title": "Other", "brief": "do it", "worker": " "}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(started["started"], true);
        let input = json!({"tasks": [
            {"key": "a", "title": "A", "project": "/p", "goal": "g", "doneWhen": "d", "worker": "w2"},
            {"key": "b", "title": "B", "project": "/p", "goal": "g", "doneWhen": "d"}
        ]});
        assert!(PlanTasksTool
            .validate_input(&input, &ctx)
            .await
            .unwrap()
            .is_valid());
        PlanTasksTool.call(input, &ctx).await.unwrap();
        assert_eq!(
            *control.workers.lock().unwrap(),
            [Some("w1".to_string()), None, Some("w2".to_string()), None]
        );
    }

    #[tokio::test]
    async fn a_plan_maps_each_key_to_its_task_id() {
        let control = Arc::new(Recorder::default());
        let ctx = context(control.clone());
        let input = json!({"tasks": [
            {"key": "api", "title": "API", "project": "/p", "goal": "g", "doneWhen": "tests pass"},
            {"key": "ui", "title": "UI", "project": "/p", "goal": "g", "doneWhen": "it renders",
             "dependsOn": ["api"]}
        ]});
        assert!(PlanTasksTool
            .validate_input(&input, &ctx)
            .await
            .unwrap()
            .is_valid());
        let planned = PlanTasksTool.call(input, &ctx).await.unwrap();
        assert_eq!(planned["tasks"]["api"], "t-api");
        assert_eq!(planned["tasks"]["ui"], "t-ui");
        CancelTaskTool
            .call(json!({"taskId": "t-ui"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            *control.calls.lock().unwrap(),
            ["plan router-session api<-,ui<-api", "cancel t-ui"]
        );
        let missing_done = json!({"tasks": [
            {"key": "a", "title": "A", "project": "/p", "goal": "g", "doneWhen": " "}
        ]});
        assert!(!PlanTasksTool
            .validate_input(&missing_done, &ctx)
            .await
            .unwrap()
            .is_valid());
    }

    #[tokio::test]
    async fn outside_a_router_session_the_tools_refuse() {
        let ctx = ToolContext::new().with_session_id("plain");
        let error = RecallTool.call(json!({}), &ctx).await.unwrap_err();
        assert!(error.to_string().contains("not the router"), "{error}");
    }
}
