//! `ProfileSwitch` — ask the user to move this session onto a saved profile.
//!
//! A profile bundles provider, model, agent backend,
//! permission mode and tool surface; `/profile <name>` is the user switching
//! it themselves, and this is the model asking them to.
//!
//! # This tool proposes; it never applies
//!
//! Nothing here reads the profile store or touches the session, and that is
//! structural rather than a simplification: `rebon-config` sits *above*
//! `rebon-tool` in the dependency graph, and the live session — its runtime
//! model, its agent backend, its tool-filter handle — lives in the front end.
//! So the tool carries a name and a reason, and the front end resolves it,
//! diffs it against what the session is actually running, shows that diff, and
//! applies it only if the user says yes.
//!
//! That split is also the safer one. The diff the user approves is computed
//! from the session's real state rather than from anything the model asserted,
//! so a model cannot describe one switch and perform another.
//!
//! # Approval is not optional for this tool
//!
//! `check_permissions` always asks, and the engine's mode gate has a carve-out
//! so `bypassPermissions` and `auto` cannot run it unprompted — a tool whose
//! entire purpose is a user decision has nothing left if the prompt is
//! skipped. The profile design rules out a model changing the permission mode
//! without a prompt, and this is half of where that is enforced; the other
//! half is the front end, which refuses a profile carrying
//! `bypassPermissions` outright.

use async_trait::async_trait;
use serde_json::{json, Value};

use rebon_tool::{Tool, ToolContext};

use crate::{PROFILE_SWITCH_APPLIED_KEY, PROFILE_SWITCH_TOOL_NAME};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};

const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct ProfileSwitchTool;

#[async_trait]
impl Tool for ProfileSwitchTool {
    fn id(&self) -> ToolId {
        ToolId::new(PROFILE_SWITCH_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Ask the user to switch this session onto a saved profile.\n\
         \n\
         A profile is a named bundle of provider, model, agent backend, permission mode and \
         tool surface, saved at ~/.rebon/profiles/. Use this when the work has clearly moved \
         into a mode the user has a profile for — long-form writing, review, cheap bulk edits.\n\
         \n\
         The user always sees a prompt showing exactly what would change, field by field, \
         against what the session is running right now, and nothing changes unless they \
         approve it. Say plainly in `reason` why the switch helps; that text is shown to them \
         and is the only thing arguing your case.\n\
         \n\
         Only profiles the user has already saved can be named — this tool does not create \
         them. A profile that turns permission prompts off is refused outright, however it \
         was saved: the user can apply one of those themselves, and you cannot ask them to.\n\
         \n\
         Anything the profile does not declare is left exactly as it is, so a switch is \
         usually narrower than it sounds. Do not call this repeatedly; if the user declines, \
         carry on in the current setup."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "profile": {
                    "type": "string",
                    "description": "Name or id of a saved profile, as shown by /profile list."
                },
                "reason": {
                    "type": "string",
                    "description": "Why this switch helps, in one or two sentences. Shown to the user in the approval prompt."
                }
            },
            "required": ["profile", "reason"],
            "additionalProperties": true
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        // A sub-agent's profile switch would move the *parent* session it was
        // spawned from, since that is the only session there is. A worker is
        // not the right place for a decision about the user's whole setup.
        if context.team_identity().is_some() || context.agent_id().is_some() {
            return Ok(ValidationOutcome::invalid(
                "ProfileSwitch cannot be used from a sub-agent — only the session the user is sitting in front of can change profile.",
                INVALID_INPUT_CODE,
            ));
        }
        if non_empty(input, "profile").is_none() {
            return Ok(ValidationOutcome::invalid(
                "ProfileSwitch needs `profile`: the name of a saved profile, as shown by /profile list.",
                INVALID_INPUT_CODE,
            ));
        }
        if non_empty(input, "reason").is_none() {
            return Ok(ValidationOutcome::invalid(
                "ProfileSwitch needs `reason`: it is what the user reads when deciding, so an empty one asks them to approve a change with no case made for it.",
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
        let profile = non_empty(input, "profile").unwrap_or_default();
        let reason = non_empty(input, "reason").unwrap_or_default();
        Ok(PermissionDecision::ask(
            PermissionRequest::new(format!("Switch to profile \"{profile}\"?"), reason)
                .with_options(["allow_once", "reject_once"])
                // The front end resolves the name against the store and replaces
                // this with the real diff. Carrying the request through metadata
                // as well keeps a surface that does not understand profiles from
                // rendering a blank prompt.
                .with_metadata(json!({
                    "kind": "profileSwitch",
                    "profile": profile,
                })),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        // Approval alone does not mean the switch happened. Only a front end
        // that can reach the live session writes this key, so its absence
        // means the call was approved somewhere that cannot apply profiles —
        // and reporting success there would tell the model the session had
        // moved when it had not.
        let Some(applied) = input.get(PROFILE_SWITCH_APPLIED_KEY) else {
            return Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(
                    "This surface cannot apply a profile — the session was left as it was. Ask the user to run `/profile use <name>` in the terminal instead."
                ),
            });
        };
        Ok(json!({
            "profileSwitched": true,
            "profile": non_empty(&input, "profile").unwrap_or_default(),
            "applied": applied.clone(),
        }))
    }
}

fn non_empty(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tool::TeamIdentityContext;
    use rebon_tools_core::PermissionBehavior;

    fn context() -> ToolContext {
        ToolContext::default()
    }

    #[tokio::test]
    async fn a_switch_always_asks_and_carries_the_model_s_reason() {
        let input = json!({"profile": "writing", "reason": "the next few turns are prose"});

        let decision = ProfileSwitchTool
            .check_permissions(&input, &context())
            .await
            .unwrap();

        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        let request = decision.request.expect("asks");
        assert!(request.title.contains("writing"), "{}", request.title);
        // The reason is the whole case the user reads; it must reach them
        // verbatim rather than being summarized into the title.
        assert_eq!(request.message, "the next few turns are prose");
    }

    #[tokio::test]
    async fn a_reasonless_switch_is_refused_before_it_reaches_the_user() {
        let outcome = ProfileSwitchTool
            .validate_input(&json!({"profile": "writing"}), &context())
            .await
            .unwrap();

        assert!(!outcome.is_valid(), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_sub_agent_cannot_move_the_session_it_was_spawned_from() {
        let context = context().with_team_identity(TeamIdentityContext {
            agent_id: "worker-1".into(),
            agent_name: "worker".into(),
            team_name: "team".into(),
            permission_mode: None,
        });

        let outcome = ProfileSwitchTool
            .validate_input(&json!({"profile": "writing", "reason": "prose"}), &context)
            .await
            .unwrap();

        assert!(!outcome.is_valid(), "{outcome:?}");
    }

    #[tokio::test]
    async fn approval_from_a_surface_that_cannot_apply_reports_failure_not_success() {
        // Approved, but nothing injected the report — so nothing applied it.
        let result = ProfileSwitchTool
            .call(json!({"profile": "writing", "reason": "prose"}), &context())
            .await;

        let err = result.expect_err("must not claim the session moved");
        assert!(err.to_string().contains("/profile use"), "{err}");
    }

    #[tokio::test]
    async fn an_applied_report_is_handed_back_to_the_model() {
        let input = json!({
            "profile": "writing",
            "reason": "prose",
            "applied": ["model -> vendor-flash", "tools -> Read, Edit"],
        });

        let output = ProfileSwitchTool.call(input, &context()).await.unwrap();

        assert_eq!(output["profileSwitched"], json!(true));
        assert_eq!(output["applied"][0], json!("model -> vendor-flash"));
    }
}
