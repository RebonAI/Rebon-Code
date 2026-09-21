//! `ProfileSave` — draft a profile for the user to accept.
//!
//! The model proposes a named bundle — provider, model,
//! agent backend, permission mode, tool surface — and the user sees every
//! field it would write before anything reaches `~/.rebon/profiles/`.
//!
//! Same split as [`crate::profile_switch`]: this tool carries a proposal and
//! nothing else. It cannot write the file, because `rebon-config` sits above
//! this crate in the dependency graph, and it should not want to: the front
//! end validates the fields against the providers that actually exist and
//! writes only after the user approves.
//!
//! # One thing the model may not write
//!
//! `permissionMode: "bypassPermissions"` is refused at validation, before the
//! user is ever asked. A saved profile outlives the conversation that created
//! it, and the user applies it later by name — so a model-authored profile
//! carrying bypass would be a way to get the prompts turned off in some future
//! session, laundered through a file the user has stopped reading closely. The
//! user can still write one themselves with `/profile save`; that is their
//! decision about their own machine, made with the mode in front of them.

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use rebon_tool::{Tool, ToolContext};

use crate::{PROFILE_SAVE_TOOL_NAME, PROFILE_SAVE_WRITTEN_KEY};
use rebon_tools_core::{
    PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};

/// The fields a profile may declare, in the order the prompt lists them.
///
/// A profile that declares none of these changes nothing, which is refused:
/// applying it would report success while leaving the session untouched.
pub const PROFILE_SAVE_DECLARABLE_FIELDS: &[&str] =
    &["provider", "model", "agent", "permissionMode", "tools"];

const INVALID_INPUT_CODE: i64 = 400;
const BYPASS_MODE: &str = "bypassPermissions";

#[derive(Debug, Clone, Default)]
pub struct ProfileSaveTool;

#[async_trait]
impl Tool for ProfileSaveTool {
    fn id(&self) -> ToolId {
        ToolId::new(PROFILE_SAVE_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Propose saving a named profile: a bundle of provider, model, agent backend, permission \
         mode and tool surface the user can switch to later with /profile.\n\
         \n\
         The user sees every field before anything is written, and nothing is written unless \
         they approve. Use this when a setup has proven useful and is worth a name — not to \
         record whatever the session happens to be running.\n\
         \n\
         Declare only the fields the profile should change. Anything you leave out is left \
         alone when the profile is applied, so a writing profile that narrows tools and moves \
         the model should say those two things and nothing else — adding fields that merely \
         restate the current setup makes the profile fail the day the user changes providers.\n\
         \n\
         `provider` and `model` must name a provider the user actually has (see /provider \
         list) and a model it serves. `permissionMode` may be default, plan, acceptEdits, auto \
         or dontAsk; bypassPermissions is refused."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "What to call the profile, e.g. \"writing\". Becomes its id."
                },
                "description": {
                    "type": "string",
                    "description": "One line on what the profile is for. Shown in /profile list."
                },
                "provider": {
                    "type": "string",
                    "description": "Provider to activate, by the name /provider list shows. Omit to leave the provider alone."
                },
                "model": {
                    "type": "string",
                    "description": "Model to run. Must be one the provider serves. Omit to leave the model alone."
                },
                "agent": {
                    "type": "string",
                    "description": "Agent backend — `local` for Rebon's own engine, or a configured agent's id. Omit to leave it alone."
                },
                "permissionMode": {
                    "type": "string",
                    "enum": ["default", "plan", "acceptEdits", "auto", "dontAsk"],
                    "description": "Permission mode to switch into. Omit to leave it alone."
                },
                "tools": {
                    "type": "object",
                    "description": "Narrow the tool surface. `allow` keeps only the named tools; `deny` removes them. A profile can only ever take tools away.",
                    "properties": {
                        "allow": { "type": "array", "items": { "type": "string" } },
                        "deny": { "type": "array", "items": { "type": "string" } }
                    },
                    "additionalProperties": false
                }
            },
            "required": ["name"],
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
        if context.team_identity().is_some() || context.agent_id().is_some() {
            return Ok(ValidationOutcome::invalid(
                "ProfileSave cannot be used from a sub-agent — a profile is the user's own setup, and a worker is not where that decision belongs.",
                INVALID_INPUT_CODE,
            ));
        }
        if non_empty(input, "name").is_none() {
            return Ok(ValidationOutcome::invalid(
                "ProfileSave needs `name`: what the profile should be called.",
                INVALID_INPUT_CODE,
            ));
        }
        if declared_fields(input).is_empty() {
            return Ok(ValidationOutcome::invalid(
                format!(
                    "ProfileSave needs at least one of {}. A profile declaring none of them changes nothing, and applying it would report success while leaving the session exactly as it was.",
                    PROFILE_SAVE_DECLARABLE_FIELDS.join(", ")
                ),
                INVALID_INPUT_CODE,
            ));
        }
        if non_empty(input, "permissionMode")
            .is_some_and(|mode| mode.eq_ignore_ascii_case(BYPASS_MODE))
        {
            return Ok(ValidationOutcome::invalid(
                "ProfileSave will not write a profile that turns permission prompts off. The user can save one themselves with /profile save while in that mode; it is not something to leave on disk for them to apply later.",
                INVALID_INPUT_CODE,
            ));
        }
        if let Some(tools) = input.get("tools") {
            if let Some(reason) = tool_surface_problem(tools) {
                return Ok(ValidationOutcome::invalid(reason, INVALID_INPUT_CODE));
            }
        }
        Ok(ValidationOutcome::valid())
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let name = non_empty(input, "name").unwrap_or_default();
        let description = non_empty(input, "description")
            .unwrap_or_else(|| "Review what this profile would declare.".to_string());
        Ok(PermissionDecision::ask(
            PermissionRequest::new(format!("Save profile \"{name}\"?"), description)
                .with_options(["allow_once", "reject_once"])
                // The front end re-reads the fields off the input and checks
                // them against the providers that exist; this is only so a
                // surface that knows nothing about profiles still renders
                // something truthful.
                .with_metadata(json!({
                    "kind": "profileSave",
                    "profile": name,
                })),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
        let Some(written) = input.get(PROFILE_SAVE_WRITTEN_KEY) else {
            return Err(ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(
                    "This surface cannot write a profile — nothing was saved. Ask the user to run `/profile save <name>` in the terminal instead."
                ),
            });
        };
        Ok(json!({
            "profileSaved": true,
            "profile": non_empty(&input, "name").unwrap_or_default(),
            "written": written.clone(),
        }))
    }
}

/// Which declarable fields carry a usable value.
///
/// An explicit `null` counts as absent, so a model spelling out "leave this
/// alone" the long way gets the same answer as omitting it.
pub fn declared_fields(input: &Value) -> Vec<&'static str> {
    PROFILE_SAVE_DECLARABLE_FIELDS
        .iter()
        .copied()
        .filter(|field| match input.get(*field) {
            None | Some(Value::Null) => false,
            Some(Value::String(text)) => !text.trim().is_empty(),
            Some(Value::Object(map)) => !map.is_empty(),
            Some(_) => true,
        })
        .collect()
}

fn tool_surface_problem(tools: &Value) -> Option<String> {
    let Some(map) = tools.as_object() else {
        return Some(
            "ProfileSave `tools` must be an object with `allow` and/or `deny` arrays.".into(),
        );
    };
    if map.is_empty() {
        return Some(
            "ProfileSave `tools` is empty. Omit it to leave the tool surface alone.".into(),
        );
    }
    for key in ["allow", "deny"] {
        if let Some(value) = map.get(key) {
            let Some(entries) = value.as_array() else {
                return Some(format!(
                    "ProfileSave `tools.{key}` must be an array of tool names."
                ));
            };
            if entries.iter().any(|entry| entry.as_str().is_none()) {
                return Some(format!(
                    "ProfileSave `tools.{key}` must contain tool names as strings."
                ));
            }
            // An empty allow list hides every tool, which is a session that
            // can do nothing rather than a narrowed one.
            if key == "allow" && entries.is_empty() {
                return Some(
                    "ProfileSave `tools.allow` is empty, which would hide every tool. Name the tools to keep, or omit `tools` entirely.".into(),
                );
            }
        }
    }
    if unknown_keys(map).next().is_some() {
        return Some(format!(
            "ProfileSave `tools` accepts only `allow` and `deny`; got {}.",
            unknown_keys(map).collect::<Vec<_>>().join(", ")
        ));
    }
    None
}

fn unknown_keys(map: &Map<String, Value>) -> impl Iterator<Item = &str> {
    map.keys()
        .map(String::as_str)
        .filter(|key| !matches!(*key, "allow" | "deny"))
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
    use rebon_tools_core::PermissionBehavior;

    fn context() -> ToolContext {
        ToolContext::default()
    }

    async fn validate(input: Value) -> ValidationOutcome {
        ProfileSaveTool
            .validate_input(&input, &context())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_profile_that_would_change_nothing_is_refused() {
        let outcome = validate(json!({"name": "empty", "description": "just a label"})).await;

        assert!(!outcome.is_valid(), "{outcome:?}");
    }

    #[tokio::test]
    async fn an_explicit_null_reads_as_leaving_the_field_alone() {
        // Declaring everything as null is the same as declaring nothing, and
        // must not slip past the "changes nothing" guard.
        let outcome = validate(json!({
            "name": "nulls",
            "provider": null,
            "model": null,
            "tools": null,
        }))
        .await;

        assert!(!outcome.is_valid(), "{outcome:?}");
        assert_eq!(
            declared_fields(&json!({"model": "x", "provider": null})),
            vec!["model"]
        );
    }

    #[tokio::test]
    async fn the_model_cannot_leave_a_bypass_profile_on_disk() {
        let outcome = validate(json!({
            "name": "risky",
            "permissionMode": "bypassPermissions",
        }))
        .await;

        assert!(!outcome.is_valid(), "{outcome:?}");
        // Every other mode is the model's to propose; the user still approves.
        assert!(
            validate(json!({"name": "ok", "permissionMode": "acceptEdits"}))
                .await
                .is_valid()
        );
    }

    #[tokio::test]
    async fn an_allow_list_that_hides_everything_is_refused() {
        assert!(!validate(json!({"name": "mute", "tools": {"allow": []}}))
            .await
            .is_valid());
        assert!(
            !validate(json!({"name": "odd", "tools": {"keep": ["Read"]}}))
                .await
                .is_valid()
        );
        assert!(
            validate(json!({"name": "narrow", "tools": {"allow": ["Read", "Edit"]}}))
                .await
                .is_valid()
        );
    }

    #[tokio::test]
    async fn saving_always_asks_and_names_the_profile() {
        let decision = ProfileSaveTool
            .check_permissions(
                &json!({"name": "writing", "description": "prose mode", "model": "flash"}),
                &context(),
            )
            .await
            .unwrap();

        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        let request = decision.request.expect("asks");
        assert!(request.title.contains("writing"), "{}", request.title);
        assert_eq!(request.message, "prose mode");
    }

    #[tokio::test]
    async fn approval_without_a_write_report_is_an_error_not_a_success() {
        let result = ProfileSaveTool
            .call(json!({"name": "writing", "model": "flash"}), &context())
            .await;

        let err = result.expect_err("must not claim a profile was saved");
        assert!(err.to_string().contains("/profile save"), "{err}");
    }
}
