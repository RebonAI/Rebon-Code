//! `EnterPlanMode` — requests permission to transition the session into
//! plan mode for complex tasks that need exploration and design before
//! code changes.
//!
//! The tool takes no input parameters, is read-only and
//! concurrency-safe, and its success output carries the workflow
//! instructions the model should follow while planning. Plan-mode
//! activation itself is a cooperative signal: the tool emits
//! `enteredPlanMode: true` in its output so the engine / dispatch layer
//! can propagate the mode change back to session storage. The user must
//! approve the switch first, via [`PermissionDecision::ask`] in
//! [`check_permissions`].
//!
//! The tool refuses to run for teammates and sub-agents: it checks
//! [`ToolContext::team_identity`] — teammate turns are spawned with a
//! non-empty identity, leader turns leave it unset.

use async_trait::async_trait;
use serde_json::{json, Value};

use rebon_tool::plan_mode::ENTER_PLAN_MODE_TOOL_NAME;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};

const INVALID_INPUT_CODE: i64 = 400;

/// Workflow instructions returned alongside the success message. Plan-mode
/// reminders after entry are owned by this plugin's `attachment-producers`
/// seat; entering plan mode itself uses this single deterministic workflow
/// because session attachment state has no separate interview-phase mode.
const PLAN_MODE_WORKFLOW: &str = "\n\n\
    In plan mode, you should:\n\
    1. Thoroughly explore the codebase; when exploration is needed, use only the Explore subagent rather than performing exploratory Glob, Grep, Read, or Bash calls yourself\n\
    2. Identify similar features and architectural approaches\n\
    3. Consider multiple approaches and their trade-offs\n\
    4. Do not invoke the Plan subagent; Plan Mode assigns planning, synthesis, and the final plan to you, the parent agent\n\
    5. Use AskUserQuestion only when unresolved user decisions block the plan; combine all foreseeable independent blocking decisions into one call\n\
    6. When the user's answers and your research are sufficient, stop asking and design a concrete implementation strategy yourself\n\
    7. After research and any necessary clarification, call the existing ExitPlanMode tool directly to submit the plan for approval; never replace plan submission with an extra AskUserQuestion\n\
    \n\
    Remember: DO NOT write or edit any files yet. This is a read-only \
    exploration and planning phase.";

const ENTER_PLAN_MODE_CONFIRMATION: &str =
    "Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.";

#[derive(Debug, Clone, Default)]
pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn id(&self) -> ToolId {
        ToolId::new(ENTER_PLAN_MODE_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Requests permission to enter plan mode, where the session goes read-only \
         until the user approves a written plan.\n\
         \n\
         Use it only when the user has to pick the approach before any code is \
         written: the request is genuinely ambiguous, or the change is far-reaching \
         enough that going down the wrong path would waste real work. Ordinary work \
         does not need it. Implementing a change the user described, fixing a bug, \
         investigating behaviour, or answering a question about the code should just \
         be done. If the task is clear enough to start, start; a plan nobody asked \
         for costs the user a round trip and delays the work.\n\
         \n\
         Once you are in plan mode you:\n\
         1. Thoroughly explore the codebase; when exploration is needed, use only the Explore subagent rather than performing exploratory Glob, Grep, Read, or Bash calls yourself\n\
         2. Understand existing patterns and architecture\n\
         3. Design the implementation approach yourself\n\
         4. Do not invoke the Plan subagent; Plan Mode assigns planning, synthesis, and the final plan to you, the parent agent\n\
         5. Use AskUserQuestion only for unresolved user decisions that block the plan, combining foreseeable independent decisions into one call\n\
         6. Stop asking when the user's answers and research are sufficient\n\
         7. Call the existing ExitPlanMode tool directly after research and necessary clarification; do not substitute an extra AskUserQuestion for plan submission\n\
         \n\
         DO NOT write or edit files while in plan mode — it is a read-only \
         exploration and planning phase."
    }

    fn input_schema(&self) -> ToolInputSchema {
        // No parameters, and no extra properties accepted.
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

    fn needs_permission(&self, _input: &Value) -> bool {
        // The user must approve the mode switch before the tool runs.
        true
    }

    async fn validate_input(
        &self,
        _input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        // Teammate and sub-agent turns may not enter plan mode. The guard
        // runs as a validation failure so the error is reported before
        // approval is requested.
        if context.team_identity().is_some() {
            return Ok(ValidationOutcome::invalid(
                "EnterPlanMode tool cannot be used in agent contexts",
                INVALID_INPUT_CODE,
            ));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Enter plan mode",
                "Switch to plan mode for exploration and design before making changes?",
            )
            .with_options(["allow_once", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, _input: Value, context: &ToolContext) -> ToolResult<Value> {
        // Defense-in-depth: validation already blocks this, but a
        // custom dispatch path could skip validation and reach `call`
        // directly. Keep the guard here so teammate contexts can never
        // flip into plan mode.
        if context.team_identity().is_some() {
            return Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("EnterPlanMode tool cannot be used in agent contexts"),
            });
        }

        let message = format!("{ENTER_PLAN_MODE_CONFIRMATION}{PLAN_MODE_WORKFLOW}");

        Ok(json!({
            // Surface the mode-transition signal so the dispatch layer
            // can move the session's permission mode to `plan`.
            "enteredPlanMode": true,
            "mode": "plan",
            "message": message,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::TeamIdentityContext;
    use rebon_tools_core::PermissionBehavior;

    fn tool() -> EnterPlanModeTool {
        EnterPlanModeTool
    }

    #[tokio::test]
    async fn call_returns_confirmation_and_workflow() {
        let out = tool().call(json!({}), &ToolContext::new()).await.unwrap();
        assert_eq!(out["enteredPlanMode"], json!(true));
        assert_eq!(out["mode"], json!("plan"));
        let msg = out["message"].as_str().expect("message is a string");
        assert!(msg.starts_with(ENTER_PLAN_MODE_CONFIRMATION));
        assert!(msg.contains("DO NOT write or edit any files"));
        assert!(msg.contains("combine all foreseeable independent blocking decisions"));
        assert!(msg.contains("user's answers and your research are sufficient, stop asking"));
        assert!(msg.contains("call the existing ExitPlanMode tool directly"));
        assert!(msg.contains("never replace plan submission with an extra AskUserQuestion"));
        assert!(msg.contains("use only the Explore subagent"));
        assert!(msg.contains("Do not invoke the Plan subagent"));
        assert!(msg.contains("final plan to you, the parent agent"));
    }

    #[tokio::test]
    async fn validate_rejects_teammate_context() {
        let context = ToolContext::new().with_team_identity(TeamIdentityContext {
            agent_id: "alice@alpha".into(),
            agent_name: "alice".into(),
            team_name: "alpha".into(),
            permission_mode: Some("default".into()),
        });
        let outcome = tool().validate_input(&json!({}), &context).await.unwrap();
        assert!(!outcome.is_valid());
        assert_eq!(outcome.error_code, Some(INVALID_INPUT_CODE));
    }

    #[tokio::test]
    async fn call_also_rejects_teammate_context_defensively() {
        let context = ToolContext::new().with_team_identity(TeamIdentityContext {
            agent_id: "alice@alpha".into(),
            agent_name: "alice".into(),
            team_name: "alpha".into(),
            permission_mode: Some("default".into()),
        });
        let err = tool().call(json!({}), &context).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution { .. }));
    }

    #[tokio::test]
    async fn check_permissions_asks_the_user() {
        let decision = tool()
            .check_permissions(&json!({}), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        let request = decision.request.expect("ask carries a request");
        assert_eq!(request.title, "Enter plan mode");
        assert!(request.options.iter().any(|o| o == "allow_once"));
        assert!(request.options.iter().any(|o| o == "reject_once"));
    }

    #[test]
    fn tool_is_read_only_concurrency_safe_and_needs_permission() {
        assert!(tool().is_read_only(&json!({})));
        assert!(tool().is_concurrency_safe(&json!({})));
        assert!(tool().needs_permission(&json!({})));
        assert_eq!(tool().id().as_str(), "EnterPlanMode");
    }

    #[test]
    fn input_schema_is_empty_object() {
        let schema = tool().input_schema();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["additionalProperties"], json!(false));
        // No required fields, no properties — empty input object.
        assert!(schema.get("required").is_none());
    }
}
