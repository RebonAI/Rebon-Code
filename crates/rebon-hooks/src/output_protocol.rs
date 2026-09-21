//! Hook runtime protocol types and JSON/stdout parsing.
//!
//! This module owns the wire/data model for hook stdout and HTTP bodies:
//! prompt requests/responses, sync vs async JSON output, permission request
//! results, and the parser helpers that split valid JSON from plain text or
//! validation errors.

use rebon_tools_core::PermissionBehavior;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::event::HookEvent;

pub type JsonObject = Map<String, Value>;
pub type PermissionUpdate = Value;

/// What a hook said about a permission request.
///
/// Three of these are decisions and are the same three the rest of the tree
/// speaks ([`PermissionBehavior`]); they serialize to the same three strings.
/// The other two are *non*-decisions the hook protocol needs and the pipeline
/// has no word for: `passthrough` hands the request to the next hook,
/// `defer` declines to decide so the host falls back to its default flow
/// (interactive ask, policy rules, …). That is why this is its own enum
/// rather than the shared one, and why the conversions below are lossy in one
/// direction only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookPermissionBehavior {
    /// Put the request to the user.
    Ask,
    /// Refuse the request.
    Deny,
    /// Run the request.
    Allow,
    /// No decision: hand the request to the next hook.
    Passthrough,
    /// No decision: fall back to the host's default permission flow.
    Defer,
}

impl From<PermissionBehavior> for HookPermissionBehavior {
    fn from(behavior: PermissionBehavior) -> Self {
        match behavior {
            PermissionBehavior::Allow => HookPermissionBehavior::Allow,
            PermissionBehavior::Deny => HookPermissionBehavior::Deny,
            PermissionBehavior::Ask => HookPermissionBehavior::Ask,
        }
    }
}

/// A hook's answer read as a decision, or `None` when it declined to make one.
///
/// `passthrough` and `defer` are not verdicts the permission pipeline can act
/// on; collapsing them to `ask` here would silently promote "I have no
/// opinion" into "stop and prompt the user", so callers get `None` and decide
/// for themselves.
impl From<HookPermissionBehavior> for Option<PermissionBehavior> {
    fn from(behavior: HookPermissionBehavior) -> Self {
        match behavior {
            HookPermissionBehavior::Allow => Some(PermissionBehavior::Allow),
            HookPermissionBehavior::Deny => Some(PermissionBehavior::Deny),
            HookPermissionBehavior::Ask => Some(PermissionBehavior::Ask),
            HookPermissionBehavior::Passthrough | HookPermissionBehavior::Defer => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookDecision {
    Approve,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequestOption {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequest {
    pub prompt: String,
    pub message: String,
    pub options: Vec<PromptRequestOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptResponse {
    pub prompt_response: String,
    pub selected: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "lowercase")]
pub enum PermissionRequestResult {
    Allow {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "updatedInput"
        )]
        updated_input: Option<JsonObject>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "updatedPermissions"
        )]
        updated_permissions: Option<Vec<PermissionUpdate>>,
    },
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "hookEventName")]
pub enum HookSpecificOutput {
    #[serde(rename = "PreToolUse")]
    PreToolUse {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "permissionDecision"
        )]
        permission_decision: Option<HookPermissionBehavior>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "permissionDecisionReason"
        )]
        permission_decision_reason: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "updatedInput"
        )]
        updated_input: Option<JsonObject>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
        /// `sessionTitle` from the Claude hooks protocol — the hook
        /// can rename the session on first prompt submit. Carried
        /// through to the host so it can update its UI label.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "sessionTitle"
        )]
        session_title: Option<String>,
    },
    #[serde(rename = "SessionStart")]
    SessionStart {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "initialUserMessage"
        )]
        initial_user_message: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "watchPaths"
        )]
        watch_paths: Option<Vec<String>>,
    },
    #[serde(rename = "SessionEnd")]
    SessionEnd {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "goalCompleted"
        )]
        goal_completed: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "continuationPrompt"
        )]
        continuation_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "Setup")]
    Setup {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "SubagentStart")]
    SubagentStart {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "PostToolUse")]
    PostToolUse {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "updatedMCPToolOutput"
        )]
        updated_mcp_tool_output: Option<Value>,
    },
    #[serde(rename = "PostToolUseFailure")]
    PostToolUseFailure {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "PermissionDenied")]
    PermissionDenied {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry: Option<bool>,
    },
    #[serde(rename = "Notification")]
    Notification {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "additionalContext"
        )]
        additional_context: Option<String>,
    },
    #[serde(rename = "PermissionRequest")]
    PermissionRequest { decision: PermissionRequestResult },
    #[serde(rename = "Elicitation")]
    Elicitation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<ElicitationAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<JsonObject>,
    },
    #[serde(rename = "ElicitationResult")]
    ElicitationResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<ElicitationAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<JsonObject>,
    },
    #[serde(rename = "CwdChanged")]
    CwdChanged {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "watchPaths"
        )]
        watch_paths: Option<Vec<String>>,
    },
    #[serde(rename = "FileChanged")]
    FileChanged {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "watchPaths"
        )]
        watch_paths: Option<Vec<String>>,
    },
    #[serde(rename = "WorktreeCreate")]
    WorktreeCreate {
        #[serde(rename = "worktreePath")]
        worktree_path: String,
    },
}

impl HookSpecificOutput {
    pub const fn event_name(&self) -> HookEvent {
        match self {
            HookSpecificOutput::PreToolUse { .. } => HookEvent::PreToolUse,
            HookSpecificOutput::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            HookSpecificOutput::SessionStart { .. } => HookEvent::SessionStart,
            HookSpecificOutput::SessionEnd { .. } => HookEvent::SessionEnd,
            HookSpecificOutput::Setup { .. } => HookEvent::Setup,
            HookSpecificOutput::SubagentStart { .. } => HookEvent::SubagentStart,
            HookSpecificOutput::PostToolUse { .. } => HookEvent::PostToolUse,
            HookSpecificOutput::PostToolUseFailure { .. } => HookEvent::PostToolUseFailure,
            HookSpecificOutput::PermissionDenied { .. } => HookEvent::PermissionDenied,
            HookSpecificOutput::Notification { .. } => HookEvent::Notification,
            HookSpecificOutput::PermissionRequest { .. } => HookEvent::PermissionRequest,
            HookSpecificOutput::Elicitation { .. } => HookEvent::Elicitation,
            HookSpecificOutput::ElicitationResult { .. } => HookEvent::ElicitationResult,
            HookSpecificOutput::CwdChanged { .. } => HookEvent::CwdChanged,
            HookSpecificOutput::FileChanged { .. } => HookEvent::FileChanged,
            HookSpecificOutput::WorktreeCreate { .. } => HookEvent::WorktreeCreate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SyncHookJsonOutput {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "continue")]
    pub r#continue: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "suppressOutput"
    )]
    pub suppress_output: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "stopReason"
    )]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<HookDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "systemMessage"
    )]
    pub system_message: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "hookSpecificOutput"
    )]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AsyncHookJsonOutput {
    #[serde(rename = "async")]
    pub is_async: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "asyncTimeout"
    )]
    pub async_timeout: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookJsonOutput {
    Async(AsyncHookJsonOutput),
    Sync(SyncHookJsonOutput),
}

pub fn is_sync_hook_json_output(json: &HookJsonOutput) -> bool {
    matches!(json, HookJsonOutput::Sync(_))
}

pub fn is_async_hook_json_output(json: &HookJsonOutput) -> bool {
    matches!(json, HookJsonOutput::Async(_))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookJsonValidationError {
    pub message: String,
}

impl std::fmt::Display for HookJsonValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for HookJsonValidationError {}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHookOutput {
    pub json: Option<HookJsonOutput>,
    pub plain_text: Option<String>,
    pub validation_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValidateHookJsonResult {
    Json(HookJsonOutput),
    ValidationError(String),
}

pub fn validate_hook_json(json_string: &str) -> ValidateHookJsonResult {
    let parsed: Value = match serde_json::from_str(json_string) {
        Ok(value) => value,
        Err(error) => {
            return ValidateHookJsonResult::ValidationError(format!(
                "Hook JSON output validation failed:\n  - invalid JSON: {error}"
            ))
        }
    };

    match serde_json::from_value::<HookJsonOutput>(parsed.clone()) {
        Ok(json) => ValidateHookJsonResult::Json(json),
        Err(error) => ValidateHookJsonResult::ValidationError(format!(
            "Hook JSON output validation failed:\n  - {error}\n\nThe hook's output was: {}",
            serde_json::to_string_pretty(&parsed).expect("serde_json::Value always serializes")
        )),
    }
}

pub fn parse_hook_output(stdout: &str) -> ParsedHookOutput {
    let trimmed = stdout.trim();
    if !trimmed.starts_with('{') {
        return ParsedHookOutput {
            json: None,
            plain_text: Some(stdout.to_string()),
            validation_error: None,
        };
    }

    match validate_hook_json(trimmed) {
        ValidateHookJsonResult::Json(json) => ParsedHookOutput {
            json: Some(json),
            plain_text: None,
            validation_error: None,
        },
        ValidateHookJsonResult::ValidationError(error) => ParsedHookOutput {
            json: None,
            plain_text: Some(stdout.to_string()),
            validation_error: Some(format!("{error}\n\nExpected schema:\n{{\"async\":true}} | {{\"continue\":false,\"hookSpecificOutput\":{{...}}}}")),
        },
    }
}

pub fn parse_http_hook_output(body: &str) -> ParsedHookOutput {
    let trimmed = body.trim();

    if trimmed.is_empty() {
        return ParsedHookOutput {
            json: Some(HookJsonOutput::Sync(SyncHookJsonOutput::default())),
            plain_text: None,
            validation_error: None,
        };
    }

    if !trimmed.starts_with('{') {
        let preview = if trimmed.chars().count() > 200 {
            let prefix: String = trimmed.chars().take(200).collect();
            format!("{prefix}…")
        } else {
            trimmed.to_string()
        };
        return ParsedHookOutput {
            json: None,
            plain_text: None,
            validation_error: Some(format!(
                "HTTP hook must return JSON, but got non-JSON response body: {preview}"
            )),
        };
    }

    match validate_hook_json(trimmed) {
        ValidateHookJsonResult::Json(json) => ParsedHookOutput {
            json: Some(json),
            plain_text: None,
            validation_error: None,
        },
        ValidateHookJsonResult::ValidationError(error) => ParsedHookOutput {
            json: None,
            plain_text: None,
            validation_error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_hook_output_treats_non_json_as_plain_text() {
        let parsed = parse_hook_output("hello\nworld");
        assert_eq!(parsed.plain_text.as_deref(), Some("hello\nworld"));
        assert!(parsed.json.is_none());
        assert!(parsed.validation_error.is_none());
    }

    #[test]
    fn parse_hook_output_splits_async_and_sync_json() {
        let async_parsed = parse_hook_output(r#"{"async":true,"asyncTimeout":12}"#);
        assert!(matches!(async_parsed.json, Some(HookJsonOutput::Async(_))));

        let sync_parsed = parse_hook_output(r#"{"decision":"block"}"#);
        assert!(matches!(sync_parsed.json, Some(HookJsonOutput::Sync(_))));
    }

    #[test]
    fn parse_hook_output_returns_validation_error_for_invalid_shape() {
        let parsed =
            parse_hook_output(r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest"}}"#);
        assert!(parsed.json.is_none());
        assert!(parsed.plain_text.is_some());
        assert!(parsed
            .validation_error
            .as_deref()
            .unwrap()
            .contains("Expected schema"));
    }

    #[test]
    fn parse_http_hook_output_accepts_empty_body_as_empty_sync_json() {
        let parsed = parse_http_hook_output("   ");
        assert!(matches!(parsed.json, Some(HookJsonOutput::Sync(_))));
        assert!(parsed.validation_error.is_none());
    }

    #[test]
    fn parse_http_hook_output_rejects_non_json_body() {
        let parsed = parse_http_hook_output("<html>nope</html>");
        assert!(parsed.json.is_none());
        assert!(parsed
            .validation_error
            .as_deref()
            .unwrap()
            .contains("must return JSON"));
    }

    #[test]
    fn hook_specific_output_event_name_round_trips() {
        let output = HookSpecificOutput::WorktreeCreate {
            worktree_path: "/tmp/wt".into(),
        };
        assert_eq!(output.event_name(), HookEvent::WorktreeCreate);
    }

    #[test]
    fn validate_hook_json_reports_invalid_json() {
        match validate_hook_json("{not-json") {
            ValidateHookJsonResult::ValidationError(error) => {
                assert!(error.contains("invalid JSON"));
            }
            ValidateHookJsonResult::Json(json) => panic!("expected validation error, got {json:?}"),
        }
    }

    #[test]
    fn validate_hook_json_accepts_async_false_as_async_shape() {
        match validate_hook_json(r#"{"async":false}"#) {
            ValidateHookJsonResult::Json(HookJsonOutput::Async(output)) => {
                assert!(!output.is_async);
                assert_eq!(output.async_timeout, None);
            }
            other => panic!("expected async output, got {other:?}"),
        }
    }

    #[test]
    fn parse_hook_output_trims_before_json_detection_but_preserves_plain_text() {
        let parsed = parse_hook_output("\n  {\"continue\":false,\"stopReason\":\"halt\"}\n");
        assert!(matches!(parsed.json, Some(HookJsonOutput::Sync(_))));
        assert!(parsed.plain_text.is_none());

        let plain = parse_hook_output("  plain text  ");
        assert_eq!(plain.plain_text.as_deref(), Some("  plain text  "));
    }

    #[test]
    fn parse_hook_output_invalid_json_preserves_original_plain_text() {
        let stdout = " {bad-json";
        let parsed = parse_hook_output(stdout);

        assert!(parsed.json.is_none());
        assert_eq!(parsed.plain_text.as_deref(), Some(stdout));
        assert!(parsed.validation_error.unwrap().contains("Expected schema"));
    }

    #[test]
    fn parse_http_hook_output_truncates_long_non_json_body_by_chars() {
        let body = "é".repeat(250);
        let parsed = parse_http_hook_output(&body);

        let error = parsed.validation_error.unwrap();
        assert!(error.contains("must return JSON"));
        assert!(error.contains('…'));
        let preview = error.split("body: ").nth(1).unwrap();
        assert_eq!(preview.chars().count(), 201);
    }

    #[test]
    fn parse_http_hook_output_rejects_json_wrong_shape_without_plain_text() {
        let parsed = parse_http_hook_output(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest"}}"#,
        );

        assert!(parsed.json.is_none());
        assert!(parsed.plain_text.is_none());
        assert!(parsed
            .validation_error
            .unwrap()
            .contains("validation failed"));
    }

    #[test]
    fn sync_hook_json_output_round_trips_all_top_level_fields() {
        let json = json!({
            "continue": false,
            "suppressOutput": true,
            "stopReason": "stop now",
            "decision": "approve",
            "reason": "looks safe",
            "systemMessage": "status text",
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": "context",
                "sessionTitle": "renamed"
            }
        });

        let parsed: SyncHookJsonOutput = serde_json::from_value(json).unwrap();

        assert_eq!(parsed.r#continue, Some(false));
        assert_eq!(parsed.suppress_output, Some(true));
        assert_eq!(parsed.stop_reason.as_deref(), Some("stop now"));
        assert_eq!(parsed.decision, Some(HookDecision::Approve));
        assert_eq!(parsed.reason.as_deref(), Some("looks safe"));
        assert_eq!(parsed.system_message.as_deref(), Some("status text"));
        assert!(matches!(
            parsed.hook_specific_output,
            Some(HookSpecificOutput::UserPromptSubmit {
                session_title: Some(ref title),
                ..
            }) if title == "renamed"
        ));
    }

    #[test]
    fn permission_request_deny_round_trips_message_and_interrupt() {
        let json = json!({
            "hookEventName": "PermissionRequest",
            "decision": {
                "behavior": "deny",
                "message": "no",
                "interrupt": true
            }
        });

        let parsed: HookSpecificOutput = serde_json::from_value(json).unwrap();

        assert!(matches!(
            parsed,
            HookSpecificOutput::PermissionRequest {
                decision: PermissionRequestResult::Deny {
                    message: Some(ref message),
                    interrupt: Some(true),
                }
            } if message == "no"
        ));
    }

    #[test]
    fn permission_behavior_defer_deserializes() {
        let behavior: HookPermissionBehavior = serde_json::from_str("\"defer\"").unwrap();
        assert_eq!(behavior, HookPermissionBehavior::Defer);
    }

    /// The hook JSON protocol is an outward contract: these five strings are
    /// what third-party hooks write on stdout, and the first three must stay
    /// byte-identical to the shared [`PermissionBehavior`] wire form.
    #[test]
    fn hook_permission_behavior_wire_form_is_fixed() {
        for (behavior, wire) in [
            (HookPermissionBehavior::Allow, "\"allow\""),
            (HookPermissionBehavior::Deny, "\"deny\""),
            (HookPermissionBehavior::Ask, "\"ask\""),
            (HookPermissionBehavior::Passthrough, "\"passthrough\""),
            (HookPermissionBehavior::Defer, "\"defer\""),
        ] {
            assert_eq!(serde_json::to_string(&behavior).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<HookPermissionBehavior>(wire).unwrap(),
                behavior
            );
        }
        for behavior in [
            PermissionBehavior::Allow,
            PermissionBehavior::Deny,
            PermissionBehavior::Ask,
        ] {
            assert_eq!(
                serde_json::to_string(&HookPermissionBehavior::from(behavior)).unwrap(),
                serde_json::to_string(&behavior).unwrap()
            );
        }
    }

    /// A hook that declines to decide must not be read as a decision.
    #[test]
    fn non_decisions_convert_to_none() {
        for (hook, decision) in [
            (
                HookPermissionBehavior::Allow,
                Some(PermissionBehavior::Allow),
            ),
            (HookPermissionBehavior::Deny, Some(PermissionBehavior::Deny)),
            (HookPermissionBehavior::Ask, Some(PermissionBehavior::Ask)),
            (HookPermissionBehavior::Passthrough, None),
            (HookPermissionBehavior::Defer, None),
        ] {
            assert_eq!(Option::<PermissionBehavior>::from(hook), decision);
        }
    }

    #[test]
    fn hook_specific_output_event_name_table_covers_new_runtime_events() {
        let cases = [
            (
                HookSpecificOutput::CwdChanged {
                    watch_paths: Some(vec!["/cwd".into()]),
                },
                HookEvent::CwdChanged,
            ),
            (
                HookSpecificOutput::FileChanged {
                    watch_paths: Some(vec!["/file".into()]),
                },
                HookEvent::FileChanged,
            ),
            (
                HookSpecificOutput::WorktreeCreate {
                    worktree_path: "/tmp/wt".into(),
                },
                HookEvent::WorktreeCreate,
            ),
        ];

        for (output, event) in cases {
            assert_eq!(output.event_name(), event);
        }
    }

    #[test]
    fn prompt_request_response_round_trip_wire_shape() {
        let request = PromptRequest {
            prompt: "Pick".into(),
            message: "Choose one".into(),
            options: vec![PromptRequestOption {
                key: "a".into(),
                label: "A".into(),
                description: None,
            }],
        };
        let request_json = serde_json::to_value(&request).unwrap();
        assert_eq!(request_json["options"][0]["key"], "a");
        assert!(request_json["options"][0].get("description").is_none());

        let response_json = json!({"prompt_response":"ok","selected":"a"});
        let response: PromptResponse = serde_json::from_value(response_json).unwrap();
        assert_eq!(response.prompt_response, "ok");
        assert_eq!(response.selected, "a");
    }

    #[test]
    fn permission_request_allow_round_trips() {
        let json = json!({
            "hookEventName": "PermissionRequest",
            "decision": {
                "behavior": "allow",
                "updatedInput": {"foo": 1},
                "updatedPermissions": [{"type": "addRules"}]
            }
        });
        let parsed: HookSpecificOutput = serde_json::from_value(json).unwrap();
        match parsed {
            HookSpecificOutput::PermissionRequest { decision } => match decision {
                PermissionRequestResult::Allow {
                    updated_input,
                    updated_permissions,
                } => {
                    assert_eq!(updated_input.unwrap().get("foo"), Some(&json!(1)));
                    assert_eq!(updated_permissions.unwrap().len(), 1);
                }
                PermissionRequestResult::Deny { .. } => panic!("expected allow decision"),
            },
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
