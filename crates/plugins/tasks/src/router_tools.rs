//! The router's tools: start, continue and stop workers, hand threads to
//! them, and look things up.
//!
//! Each tool resolves the [`WorkerControl`] its session's surface attached
//! and calls it. Only a router session sees these tools: every other session
//! filter denies them.

use std::sync::Arc;

use async_trait::async_trait;
use rebon_tool::{Tool, ToolContext, WorkerControl, WorkerDispatch};
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

/// Every router tool name.
pub const ROUTER_TOOL_NAMES: &[&str] = &[
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DispatchInput {
    project: String,
    title: String,
    brief: String,
    #[serde(default)]
    isolate: bool,
    #[serde(default)]
    model_profile: Option<String>,
}

#[async_trait]
impl Tool for DispatchWorkerTool {
    fn id(&self) -> ToolId {
        ToolId::new(DISPATCH_WORKER_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Start a worker on one task in one project. Returns its id at once; you \
         hear from it in a later digest, so do not wait.\n\
         \n\
         The worker sees only `brief`. Make it self-contained: the goal, what \
         done looks like, what is already known (with ids it can look up), and \
         anything it must not do.\n\
         \n\
         Set `isolate` for a worker that will change code while other work may \
         touch the same checkout; it then works in its own git worktree. Leave \
         it off for research and read-only work. Leave `modelProfile` out \
         unless the task clearly needs a lighter (`fast`) or stronger \
         (`reasoning`) model than the default."
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
                "isolate": {
                    "type": "boolean",
                    "description": "Work in a git worktree of its own"
                },
                "modelProfile": {
                    "type": "string",
                    "description": "A model profile such as fast or reasoning; omit for the default"
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
        let dispatched = control
            .dispatch(
                session_id,
                WorkerDispatch {
                    project: parsed.project.trim().to_string(),
                    title: parsed.title.trim().to_string(),
                    brief: parsed.brief,
                    isolate: parsed.isolate,
                    model_profile: parsed
                        .model_profile
                        .map(|profile| profile.trim().to_string())
                        .filter(|profile| !profile.is_empty()),
                },
            )
            .await
            .map_err(|error| execution_error(self, error))?;
        let mut result = json!({ "workerId": dispatched.worker_id, "started": true });
        if let Some(note) = dispatched.note {
            result["note"] = json!(note);
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ContinueInput {
    worker_id: String,
    message: String,
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
         message when its current turn ends."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "workerId": { "type": "string", "description": "The worker's id" },
                "message": { "type": "string", "description": "What to do next" }
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
            .continue_worker(session_id, worker_id, &parsed.message)
            .await
            .map_err(|error| execution_error(self, error))?;
        Ok(json!({ "workerId": worker_id, "queued": true }))
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
        "Hand a thread to a worker: later events on it (checks failing, changes \
         requested, merged, closed) go straight to that worker without passing \
         through you. A thread is a pull request, written `pr:owner/repo#12` or \
         as its URL. Handing a thread to a new worker takes it from the old one."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "thread": {
                    "type": "string",
                    "description": "pr:owner/repo#12, or the pull request's URL"
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
        "Look up an id: a worker, a thread (`pr:owner/repo#12`) or the router \
         itself. You get a short digest with the ids next to it, which you can \
         Recall in turn: a worker lists its threads and last reply, a thread \
         names its worker. With no id, lists your workers. Follow the chain \
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
    }

    #[async_trait]
    impl WorkerControl for Recorder {
        async fn dispatch(
            &self,
            router: &str,
            request: WorkerDispatch,
        ) -> Result<DispatchedWorker, String> {
            self.calls.lock().unwrap().push(format!(
                "dispatch {router} {} {} isolate={}",
                request.project, request.title, request.isolate
            ));
            Ok(DispatchedWorker {
                worker_id: "w1".into(),
                note: None,
            })
        }
        async fn continue_worker(
            &self,
            router: &str,
            worker: &str,
            message: &str,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("continue {router} {worker} {message}"));
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
    async fn outside_a_router_session_the_tools_refuse() {
        let ctx = ToolContext::new().with_session_id("plain");
        let error = RecallTool.call(json!({}), &ctx).await.unwrap_err();
        assert!(error.to_string().contains("not the router"), "{error}");
    }
}
