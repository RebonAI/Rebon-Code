//! The group tools as a Rebon session's own tools.
//!
//! Each one is `rebon-group`'s schema and handler under the tool trait. The
//! caller is never in doubt here: it is the session running the tool, so
//! nothing is read from the environment and `group_join` needs no agent or
//! session id.

use async_trait::async_trait;
use rebon_group::identity::{AgentKind, Caller};
use rebon_group::tools::{self as group_tools, ToolContext as GroupContext, ToolSpec};
use rebon_group::GroupStore;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
};
use serde_json::Value;

const PERMISSION_MESSAGE_PREVIEW_CHARS: usize = 200;

fn message_preview(text: &str) -> String {
    let mut chars = text.chars();
    let preview = chars
        .by_ref()
        .take(PERMISSION_MESSAGE_PREVIEW_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn group_send_permission_request(input: &Value) -> PermissionRequest {
    let recipient = input
        .get("to")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let kind = input.get("kind").and_then(Value::as_str).unwrap_or("note");
    let text = input.get("text").and_then(Value::as_str).unwrap_or("");
    PermissionRequest::new(
        format!("Send a group message to {recipient}"),
        format!(
            "This session wants to send a group {kind} to \"{recipient}\".\n\nMessage: {}",
            message_preview(text)
        ),
    )
}

/// One group tool.
pub struct GroupTool {
    spec: ToolSpec,
    store: GroupStore,
}

impl GroupTool {
    pub fn new(spec: ToolSpec, store: GroupStore) -> Self {
        Self { spec, store }
    }
}

/// Every group tool over `store`.
pub fn tools(store: &GroupStore) -> Vec<GroupTool> {
    group_tools::specs()
        .into_iter()
        .map(|spec| GroupTool::new(spec, store.clone()))
        .collect()
}

#[async_trait]
impl Tool for GroupTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.spec.name)
    }

    fn description(&self) -> &str {
        self.spec.description
    }

    fn input_schema(&self) -> ToolInputSchema {
        self.spec.input_schema.clone()
    }

    /// Loaded on demand like MCP tools: seven schemas the model needs only
    /// once the session is in a group, and the delivery attachment names the
    /// ones to reach for.
    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("agent group other agents messages shared memory team coordinate")
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        self.spec.read_only
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        self.spec.read_only
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        if self.spec.name != group_tools::GROUP_SEND
            || context.permission_mode().as_deref() != Some("auto")
        {
            return Ok(PermissionDecision::allow(input.clone()));
        }
        Ok(PermissionDecision::ask(
            group_send_permission_request(input),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let Some(session_id) = context.session_id().map(str::to_string) else {
            return Err(self.failed("this session has no id to join a group under"));
        };
        let root = match context.cwd() {
            Some(cwd) => cwd.to_string(),
            None => std::env::current_dir()
                .map(|dir| dir.to_string_lossy().into_owned())
                .map_err(|error| self.failed(format!("no working directory: {error}")))?,
        };
        let mut caller = Some(Caller {
            agent: AgentKind::REBON.to_string(),
            session_id,
        });
        group_tools::call(
            &mut GroupContext {
                store: &self.store,
                caller: &mut caller,
                root: &root,
            },
            self.spec.name,
            input,
        )
        .map_err(|error| self.failed(format!("{error:#}")))
    }
}

impl GroupTool {
    fn failed(&self, message: impl Into<String>) -> ToolError {
        ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!(message.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tools_core::PermissionBehavior;
    use serde_json::json;

    fn group_tools_list() -> Vec<GroupTool> {
        tools(&GroupStore::new("unused-test-root"))
    }

    fn group_send() -> GroupTool {
        group_tools_list()
            .into_iter()
            .find(|tool| tool.id().as_str() == group_tools::GROUP_SEND)
            .expect("group_send tool is always registered")
    }

    #[tokio::test]
    async fn group_send_asks_in_auto_mode_with_recipient_and_text() {
        let tool = group_send();
        let input = json!({"to": "alice", "text": "ship the parser fix", "kind": "request"});
        let context = ToolContext::new().with_permission_mode("auto");
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        let request = decision.request.expect("auto mode must carry a request");
        assert!(request.message.contains("alice"), "{}", request.message);
        assert!(
            request.message.contains("ship the parser fix"),
            "{}",
            request.message
        );
        assert!(request.message.contains("request"), "{}", request.message);
    }

    #[tokio::test]
    async fn group_send_truncates_long_text_in_the_auto_mode_request() {
        let tool = group_send();
        let long_text = format!("{}TAIL", "b".repeat(300));
        let input = json!({"to": "all", "text": long_text});
        let context = ToolContext::new().with_permission_mode("auto");
        let decision = tool.check_permissions(&input, &context).await.unwrap();
        let request = decision.request.expect("auto mode must carry a request");
        assert!(request.message.contains(&"b".repeat(200)));
        assert!(!request.message.contains("TAIL"), "{}", request.message);
    }

    #[tokio::test]
    async fn group_send_allows_every_other_mode() {
        let tool = group_send();
        let input = json!({"to": "alice", "text": "ship it"});
        let modes = [
            Some("default"),
            Some("acceptEdits"),
            Some("plan"),
            Some("bypassPermissions"),
            Some("dontAsk"),
            Some("bubble"),
            None,
        ];
        for mode in modes {
            let context = match mode {
                Some(mode) => ToolContext::new().with_permission_mode(mode),
                None => ToolContext::new(),
            };
            let decision = tool.check_permissions(&input, &context).await.unwrap();
            assert_eq!(
                decision.behavior,
                PermissionBehavior::Allow,
                "mode {mode:?} must not prompt"
            );
        }
    }

    #[tokio::test]
    async fn other_group_tools_are_not_gated_in_auto_mode() {
        let context = ToolContext::new().with_permission_mode("auto");
        for tool in group_tools_list() {
            if tool.id().as_str() == group_tools::GROUP_SEND {
                continue;
            }
            let decision = tool.check_permissions(&json!({}), &context).await.unwrap();
            assert_eq!(
                decision.behavior,
                PermissionBehavior::Allow,
                "{} must not prompt in auto mode",
                tool.id()
            );
        }
    }
}
