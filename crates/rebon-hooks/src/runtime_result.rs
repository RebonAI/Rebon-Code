//! Hook runtime result model and JSON-output projection.
//!
//! Result projection and aggregation helpers for:
//!
//! * projecting a hook's JSON output into a [`HookResult`]
//! * combining a slice of [`HookResult`]s into an [`AggregatedHookResult`]
//! * classifying exit codes and plain command results

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::event::HookEvent;
use crate::output_protocol::{
    ElicitationAction, HookDecision, HookJsonOutput, HookPermissionBehavior, HookSpecificOutput,
    JsonObject, PermissionRequestResult, SyncHookJsonOutput,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookBlockingError {
    pub blocking_error: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<JsonObject>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookResultOutcome {
    Success,
    Blocking,
    NonBlockingError,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookResult {
    pub outcome: HookResultOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_error: Option<HookBlockingError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prevent_continuation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_behavior: Option<HookPermissionBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_permission_decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_user_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_completed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_continuation_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_mcp_tool_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_request_result: Option<PermissionRequestResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation_response: Option<ElicitationResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation_result_response: Option<ElicitationResponse>,
}

impl Default for HookResult {
    fn default() -> Self {
        Self {
            outcome: HookResultOutcome::Success,
            system_message: None,
            worktree_path: None,
            blocking_error: None,
            prevent_continuation: None,
            stop_reason: None,
            permission_behavior: None,
            hook_permission_decision_reason: None,
            additional_context: None,
            session_title: None,
            initial_user_message: None,
            goal_completed: None,
            goal_continuation_prompt: None,
            goal_reason: None,
            updated_input: None,
            updated_mcp_tool_output: None,
            permission_request_result: None,
            retry: None,
            watch_paths: None,
            elicitation_response: None,
            elicitation_result_response: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AggregatedHookResult {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocking_errors: Vec<HookBlockingError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prevent_continuation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_permission_decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_behavior: Option<HookPermissionBehavior>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_contexts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_user_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_completed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_continuation_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_mcp_tool_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_request_result: Option<PermissionRequestResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation_response: Option<ElicitationResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation_result_response: Option<ElicitationResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitCodeSemantic {
    Success,
    Blocking,
    NonBlockingError,
}

pub fn classify_exit_code(exit_code: i32) -> ExitCodeSemantic {
    match exit_code {
        0 => ExitCodeSemantic::Success,
        2 => ExitCodeSemantic::Blocking,
        _ => ExitCodeSemantic::NonBlockingError,
    }
}

pub fn classify_plain_command_result(exit_code: i32, command: &str, stderr: &str) -> HookResult {
    match classify_exit_code(exit_code) {
        ExitCodeSemantic::Success => HookResult {
            outcome: HookResultOutcome::Success,
            ..HookResult::default()
        },
        ExitCodeSemantic::Blocking => HookResult {
            outcome: HookResultOutcome::Blocking,
            blocking_error: Some(HookBlockingError {
                blocking_error: format!(
                    "[{command}]: {}",
                    if stderr.is_empty() {
                        "No stderr output"
                    } else {
                        stderr
                    }
                ),
                command: command.to_string(),
            }),
            ..HookResult::default()
        },
        ExitCodeSemantic::NonBlockingError => HookResult {
            outcome: HookResultOutcome::NonBlockingError,
            ..HookResult::default()
        },
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProcessHookJsonError {
    #[error(
        "Hook returned incorrect event name: expected '{expected}' but got '{got}'. Full stdout: {full_stdout}"
    )]
    EventMismatch {
        expected: String,
        got: String,
        full_stdout: String,
    },
}

pub fn process_hook_json_output(
    json: &SyncHookJsonOutput,
    command: &str,
    expected_hook_event: Option<HookEvent>,
) -> Result<HookResult, ProcessHookJsonError> {
    let mut result = HookResult::default();

    if json.r#continue == Some(false) {
        result.prevent_continuation = Some(true);
        result.stop_reason = json.stop_reason.clone();
    }

    match json.decision {
        Some(HookDecision::Approve) => {
            result.permission_behavior = Some(HookPermissionBehavior::Allow);
        }
        Some(HookDecision::Block) => {
            result.permission_behavior = Some(HookPermissionBehavior::Deny);
            result.blocking_error = Some(HookBlockingError {
                blocking_error: json
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Blocked by hook".into()),
                command: command.to_string(),
            });
        }
        None => {}
    }

    if result.permission_behavior.is_some() {
        result.hook_permission_decision_reason = json.reason.clone();
    }

    result.system_message = json.system_message.clone();

    if let Some(specific) = &json.hook_specific_output {
        if let Some(expected) = expected_hook_event {
            let got = specific.event_name();
            if got != expected {
                return Err(ProcessHookJsonError::EventMismatch {
                    expected: expected.name().to_string(),
                    got: got.name().to_string(),
                    full_stdout: serde_json::to_string_pretty(&HookJsonOutput::Sync(json.clone()))
                        .unwrap_or_else(|_| "{}".into()),
                });
            }
        }

        match specific {
            HookSpecificOutput::PreToolUse {
                permission_decision,
                permission_decision_reason,
                updated_input,
                additional_context,
            } => {
                if let Some(decision) = permission_decision {
                    result.permission_behavior = Some(*decision);
                    if *decision == HookPermissionBehavior::Deny {
                        result.blocking_error = Some(HookBlockingError {
                            blocking_error: permission_decision_reason
                                .clone()
                                .or_else(|| json.reason.clone())
                                .unwrap_or_else(|| "Blocked by hook".into()),
                            command: command.to_string(),
                        });
                    }
                }
                result.hook_permission_decision_reason = permission_decision_reason.clone();
                result.updated_input = updated_input.clone();
                result.additional_context = additional_context.clone();
            }
            HookSpecificOutput::UserPromptSubmit {
                additional_context,
                session_title,
            } => {
                result.additional_context = additional_context.clone();
                result.session_title = session_title.clone();
            }
            HookSpecificOutput::Setup { additional_context }
            | HookSpecificOutput::SubagentStart { additional_context }
            | HookSpecificOutput::PostToolUseFailure { additional_context }
            | HookSpecificOutput::Notification { additional_context } => {
                result.additional_context = additional_context.clone();
            }
            HookSpecificOutput::SessionStart {
                additional_context,
                initial_user_message,
                watch_paths,
            } => {
                result.additional_context = additional_context.clone();
                result.initial_user_message = initial_user_message.clone();
                result.watch_paths = watch_paths.clone();
            }
            HookSpecificOutput::SessionEnd {
                goal_completed,
                continuation_prompt,
                reason,
            } => {
                result.goal_completed = *goal_completed;
                result.goal_continuation_prompt = continuation_prompt.clone();
                result.goal_reason = reason.clone();
            }
            HookSpecificOutput::PostToolUse {
                additional_context,
                updated_mcp_tool_output,
            } => {
                result.additional_context = additional_context.clone();
                result.updated_mcp_tool_output = updated_mcp_tool_output.clone();
            }
            HookSpecificOutput::PermissionDenied { retry } => {
                result.retry = *retry;
            }
            HookSpecificOutput::PermissionRequest { decision } => {
                result.permission_request_result = Some(decision.clone());
                result.permission_behavior = Some(match decision {
                    PermissionRequestResult::Allow { .. } => HookPermissionBehavior::Allow,
                    PermissionRequestResult::Deny { .. } => HookPermissionBehavior::Deny,
                });
                if let PermissionRequestResult::Allow { updated_input, .. } = decision {
                    result.updated_input = updated_input.clone();
                }
            }
            HookSpecificOutput::Elicitation { action, content } => {
                if let Some(action) = action {
                    result.elicitation_response = Some(ElicitationResponse {
                        action: *action,
                        content: content.clone(),
                    });
                    if *action == ElicitationAction::Decline {
                        result.blocking_error = Some(HookBlockingError {
                            blocking_error: json
                                .reason
                                .clone()
                                .unwrap_or_else(|| "Elicitation denied by hook".into()),
                            command: command.to_string(),
                        });
                    }
                }
            }
            HookSpecificOutput::ElicitationResult { action, content } => {
                if let Some(action) = action {
                    result.elicitation_result_response = Some(ElicitationResponse {
                        action: *action,
                        content: content.clone(),
                    });
                    if *action == ElicitationAction::Decline {
                        result.blocking_error = Some(HookBlockingError {
                            blocking_error: json
                                .reason
                                .clone()
                                .unwrap_or_else(|| "Elicitation result blocked by hook".into()),
                            command: command.to_string(),
                        });
                    }
                }
            }
            HookSpecificOutput::CwdChanged { watch_paths }
            | HookSpecificOutput::FileChanged { watch_paths } => {
                result.watch_paths = watch_paths.clone();
            }
            HookSpecificOutput::WorktreeCreate { worktree_path } => {
                result.worktree_path = Some(worktree_path.clone());
            }
        }
    }

    Ok(result)
}

pub fn aggregate_hook_results(results: &[HookResult]) -> AggregatedHookResult {
    let mut aggregated = AggregatedHookResult::default();

    for result in results {
        if let Some(blocking_error) = &result.blocking_error {
            aggregated.blocking_errors.push(blocking_error.clone());
        }
        if result.prevent_continuation == Some(true) {
            aggregated.prevent_continuation = Some(true);
        }
        if aggregated.stop_reason.is_none() {
            aggregated.stop_reason = result.stop_reason.clone();
        }
        if let Some(context) = &result.additional_context {
            aggregated.additional_contexts.push(context.clone());
        }
        if aggregated.session_title.is_none() {
            aggregated.session_title = result.session_title.clone();
        }
        if aggregated.initial_user_message.is_none() {
            aggregated.initial_user_message = result.initial_user_message.clone();
        }
        if aggregated.goal_completed.is_none() {
            aggregated.goal_completed = result.goal_completed;
        }
        if aggregated.goal_continuation_prompt.is_none() {
            aggregated.goal_continuation_prompt = result.goal_continuation_prompt.clone();
        }
        if aggregated.goal_reason.is_none() {
            aggregated.goal_reason = result.goal_reason.clone();
        }
        if let Some(paths) = &result.watch_paths {
            aggregated.watch_paths.extend(paths.iter().cloned());
        }
        if result.updated_mcp_tool_output.is_some() {
            aggregated.updated_mcp_tool_output = result.updated_mcp_tool_output.clone();
        }
        if aggregated.system_message.is_none() {
            aggregated.system_message = result.system_message.clone();
        }
        if aggregated.worktree_path.is_none() {
            aggregated.worktree_path = result.worktree_path.clone();
        }
        if result.permission_request_result.is_some() {
            aggregated.permission_request_result = result.permission_request_result.clone();
        }
        if result.retry == Some(true) {
            aggregated.retry = Some(true);
        }
        if result.elicitation_response.is_some() {
            aggregated.elicitation_response = result.elicitation_response.clone();
        }
        if result.elicitation_result_response.is_some() {
            aggregated.elicitation_result_response = result.elicitation_result_response.clone();
        }

        if let Some(behavior) = result.permission_behavior {
            // Precedence (strongest → weakest):
            //   Deny > Ask > Allow > Passthrough > Defer
            // Defer is the "no-opinion" decision — it only wins when
            // every prior hook was silent.
            let replace = match (aggregated.permission_behavior, behavior) {
                (_, HookPermissionBehavior::Deny) => true,
                (Some(HookPermissionBehavior::Deny), _) => false,
                (_, HookPermissionBehavior::Ask) => true,
                (Some(HookPermissionBehavior::Ask), _) => false,
                (_, HookPermissionBehavior::Allow) => true,
                (Some(HookPermissionBehavior::Allow), _) => false,
                (_, HookPermissionBehavior::Passthrough) => true,
                (Some(HookPermissionBehavior::Passthrough), _) => false,
                (None, HookPermissionBehavior::Defer) => true,
                (Some(_), HookPermissionBehavior::Defer) => false,
            };
            if replace {
                aggregated.permission_behavior = Some(behavior);
                aggregated.hook_permission_decision_reason =
                    result.hook_permission_decision_reason.clone();
                aggregated.updated_input = match behavior {
                    HookPermissionBehavior::Allow | HookPermissionBehavior::Ask => {
                        result.updated_input.clone()
                    }
                    HookPermissionBehavior::Deny
                    | HookPermissionBehavior::Passthrough
                    | HookPermissionBehavior::Defer => None,
                };
            }
        }

        if result.permission_behavior.is_none() && result.updated_input.is_some() {
            aggregated.updated_input = result.updated_input.clone();
        }
    }

    aggregated.watch_paths.sort();
    aggregated.watch_paths.dedup();
    aggregated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_protocol::{HookSpecificOutput, PermissionRequestResult};
    use serde_json::json;

    #[test]
    fn classify_exit_code_semantics() {
        assert_eq!(classify_exit_code(0), ExitCodeSemantic::Success);
        assert_eq!(classify_exit_code(2), ExitCodeSemantic::Blocking);
        assert_eq!(classify_exit_code(7), ExitCodeSemantic::NonBlockingError);
    }

    #[test]
    fn plain_exit_code_two_becomes_blocking_error() {
        let result = classify_plain_command_result(2, "echo hi", "denied");
        assert_eq!(result.outcome, HookResultOutcome::Blocking);
        assert_eq!(
            result.blocking_error.unwrap().blocking_error,
            "[echo hi]: denied"
        );
    }

    #[test]
    fn process_hook_json_output_extracts_pre_tool_use_fields() {
        let json = SyncHookJsonOutput {
            decision: Some(HookDecision::Approve),
            hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                permission_decision: Some(HookPermissionBehavior::Ask),
                permission_decision_reason: Some("check again".into()),
                updated_input: Some(serde_json::from_value(json!({"x": 1})).unwrap()),
                additional_context: Some("ctx".into()),
            }),
            ..SyncHookJsonOutput::default()
        };

        let result = process_hook_json_output(&json, "cmd", Some(HookEvent::PreToolUse)).unwrap();
        assert_eq!(
            result.permission_behavior,
            Some(HookPermissionBehavior::Ask)
        );
        assert_eq!(
            result.hook_permission_decision_reason.as_deref(),
            Some("check again")
        );
        assert_eq!(result.additional_context.as_deref(), Some("ctx"));
        assert_eq!(result.updated_input.unwrap().get("x"), Some(&json!(1)));
    }

    #[test]
    fn process_hook_json_output_extracts_permission_request_result() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::PermissionRequest {
                decision: PermissionRequestResult::Allow {
                    updated_input: Some(serde_json::from_value(json!({"y": 2})).unwrap()),
                    updated_permissions: None,
                },
            }),
            ..SyncHookJsonOutput::default()
        };

        let result =
            process_hook_json_output(&json, "cmd", Some(HookEvent::PermissionRequest)).unwrap();
        assert!(matches!(
            result.permission_request_result,
            Some(PermissionRequestResult::Allow { .. })
        ));
        assert_eq!(
            result.permission_behavior,
            Some(HookPermissionBehavior::Allow)
        );
        assert_eq!(result.updated_input.unwrap().get("y"), Some(&json!(2)));
    }

    #[test]
    fn process_hook_json_output_extracts_elicitation_decline_blocking_error() {
        let json = SyncHookJsonOutput {
            reason: Some("nope".into()),
            hook_specific_output: Some(HookSpecificOutput::Elicitation {
                action: Some(ElicitationAction::Decline),
                content: Some(serde_json::from_value(json!({"a": true})).unwrap()),
            }),
            ..SyncHookJsonOutput::default()
        };

        let result = process_hook_json_output(&json, "cmd", Some(HookEvent::Elicitation)).unwrap();
        assert_eq!(
            result.elicitation_response.unwrap().action,
            ElicitationAction::Decline
        );
        assert_eq!(result.blocking_error.unwrap().blocking_error, "nope");
    }

    #[test]
    fn process_hook_json_output_rejects_event_mismatch() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::FileChanged {
                watch_paths: Some(vec!["/tmp/a".into()]),
            }),
            ..SyncHookJsonOutput::default()
        };

        let err = process_hook_json_output(&json, "cmd", Some(HookEvent::CwdChanged)).unwrap_err();
        assert!(matches!(err, ProcessHookJsonError::EventMismatch { .. }));
    }

    #[test]
    fn process_hook_json_output_extracts_user_prompt_submit_session_title() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::UserPromptSubmit {
                additional_context: Some("ctx".into()),
                session_title: Some("renamed".into()),
            }),
            ..SyncHookJsonOutput::default()
        };
        let result =
            process_hook_json_output(&json, "cmd", Some(HookEvent::UserPromptSubmit)).unwrap();
        assert_eq!(result.additional_context.as_deref(), Some("ctx"));
        assert_eq!(result.session_title.as_deref(), Some("renamed"));
    }

    #[test]
    fn process_hook_json_output_extracts_continue_false_stop_reason_and_system_message() {
        let json = SyncHookJsonOutput {
            r#continue: Some(false),
            stop_reason: Some("policy stop".into()),
            system_message: Some("notify user".into()),
            ..SyncHookJsonOutput::default()
        };

        let result = process_hook_json_output(&json, "cmd", Some(HookEvent::Stop)).unwrap();

        assert_eq!(result.prevent_continuation, Some(true));
        assert_eq!(result.stop_reason.as_deref(), Some("policy stop"));
        assert_eq!(result.system_message.as_deref(), Some("notify user"));
    }

    #[test]
    fn process_hook_json_output_decision_block_uses_reason_or_default() {
        let with_reason = SyncHookJsonOutput {
            decision: Some(HookDecision::Block),
            reason: Some("custom deny".into()),
            ..SyncHookJsonOutput::default()
        };
        let result = process_hook_json_output(&with_reason, "cmd", None).unwrap();
        assert_eq!(
            result.permission_behavior,
            Some(HookPermissionBehavior::Deny)
        );
        assert_eq!(result.blocking_error.unwrap().blocking_error, "custom deny");

        let without_reason = SyncHookJsonOutput {
            decision: Some(HookDecision::Block),
            ..SyncHookJsonOutput::default()
        };
        let result = process_hook_json_output(&without_reason, "cmd", None).unwrap();
        assert_eq!(
            result.blocking_error.unwrap().blocking_error,
            "Blocked by hook"
        );
    }

    #[test]
    fn process_hook_json_output_session_start_extracts_seed_context_and_watch_paths() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::SessionStart {
                additional_context: Some("repo facts".into()),
                initial_user_message: Some("run setup".into()),
                watch_paths: Some(vec!["/a".into(), "/b".into()]),
            }),
            ..SyncHookJsonOutput::default()
        };

        let result = process_hook_json_output(&json, "cmd", Some(HookEvent::SessionStart)).unwrap();

        assert_eq!(result.additional_context.as_deref(), Some("repo facts"));
        assert_eq!(result.initial_user_message.as_deref(), Some("run setup"));
        assert_eq!(result.watch_paths.unwrap(), vec!["/a", "/b"]);
    }

    #[test]
    fn process_hook_json_output_post_tool_use_extracts_context_and_output_patch() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::PostToolUse {
                additional_context: Some("tool context".into()),
                updated_mcp_tool_output: Some(json!({"patched": true})),
            }),
            ..SyncHookJsonOutput::default()
        };

        let result = process_hook_json_output(&json, "cmd", Some(HookEvent::PostToolUse)).unwrap();

        assert_eq!(result.additional_context.as_deref(), Some("tool context"));
        assert_eq!(
            result.updated_mcp_tool_output,
            Some(json!({"patched": true}))
        );
    }

    #[test]
    fn process_hook_json_output_permission_denied_extracts_retry_false_and_true() {
        for (retry, expected) in [
            (Some(true), Some(true)),
            (Some(false), Some(false)),
            (None, None),
        ] {
            let json = SyncHookJsonOutput {
                hook_specific_output: Some(HookSpecificOutput::PermissionDenied { retry }),
                ..SyncHookJsonOutput::default()
            };

            let result =
                process_hook_json_output(&json, "cmd", Some(HookEvent::PermissionDenied)).unwrap();

            assert_eq!(result.retry, expected);
        }
    }

    #[test]
    fn process_hook_json_output_permission_request_deny_sets_behavior() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::PermissionRequest {
                decision: PermissionRequestResult::Deny {
                    message: Some("denied by hook".into()),
                    interrupt: Some(true),
                },
            }),
            ..SyncHookJsonOutput::default()
        };

        let result =
            process_hook_json_output(&json, "cmd", Some(HookEvent::PermissionRequest)).unwrap();

        assert_eq!(
            result.permission_behavior,
            Some(HookPermissionBehavior::Deny)
        );
        assert!(matches!(
            result.permission_request_result,
            Some(PermissionRequestResult::Deny {
                message: Some(ref message),
                interrupt: Some(true),
            }) if message == "denied by hook"
        ));
    }

    #[test]
    fn process_hook_json_output_elicitation_accept_and_result_cancel_do_not_block() {
        let accept = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::Elicitation {
                action: Some(ElicitationAction::Accept),
                content: Some(serde_json::from_value(json!({"name": "bon"})).unwrap()),
            }),
            ..SyncHookJsonOutput::default()
        };
        let accept_result =
            process_hook_json_output(&accept, "cmd", Some(HookEvent::Elicitation)).unwrap();
        assert_eq!(
            accept_result.elicitation_response.unwrap().action,
            ElicitationAction::Accept
        );
        assert!(accept_result.blocking_error.is_none());

        let cancel = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::ElicitationResult {
                action: Some(ElicitationAction::Cancel),
                content: None,
            }),
            ..SyncHookJsonOutput::default()
        };
        let cancel_result =
            process_hook_json_output(&cancel, "cmd", Some(HookEvent::ElicitationResult)).unwrap();
        assert_eq!(
            cancel_result.elicitation_result_response.unwrap().action,
            ElicitationAction::Cancel
        );
        assert!(cancel_result.blocking_error.is_none());
    }

    #[test]
    fn process_hook_json_output_elicitation_result_decline_blocks_with_default_reason() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::ElicitationResult {
                action: Some(ElicitationAction::Decline),
                content: None,
            }),
            ..SyncHookJsonOutput::default()
        };

        let result =
            process_hook_json_output(&json, "cmd", Some(HookEvent::ElicitationResult)).unwrap();

        assert_eq!(
            result.blocking_error.unwrap().blocking_error,
            "Elicitation result blocked by hook"
        );
    }

    #[test]
    fn process_hook_json_output_cwd_and_file_changed_extract_watch_paths() {
        for (event, output) in [
            (
                HookEvent::CwdChanged,
                HookSpecificOutput::CwdChanged {
                    watch_paths: Some(vec!["/cwd".into()]),
                },
            ),
            (
                HookEvent::FileChanged,
                HookSpecificOutput::FileChanged {
                    watch_paths: Some(vec!["/file".into()]),
                },
            ),
        ] {
            let json = SyncHookJsonOutput {
                hook_specific_output: Some(output),
                ..SyncHookJsonOutput::default()
            };

            let result = process_hook_json_output(&json, "cmd", Some(event)).unwrap();

            assert!(result.watch_paths.unwrap()[0].starts_with('/'));
        }
    }

    #[test]
    fn process_hook_json_output_worktree_create_extracts_path() {
        let json = SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::WorktreeCreate {
                worktree_path: "/tmp/rebon-wt".into(),
            }),
            ..SyncHookJsonOutput::default()
        };

        let result =
            process_hook_json_output(&json, "cmd", Some(HookEvent::WorktreeCreate)).unwrap();

        assert_eq!(result.worktree_path.as_deref(), Some("/tmp/rebon-wt"));
    }

    #[test]
    fn aggregate_hook_results_merges_context_paths_output_retry_and_system_fields() {
        let first = HookResult {
            additional_context: Some("first ctx".into()),
            system_message: Some("system one".into()),
            worktree_path: Some("/wt/one".into()),
            watch_paths: Some(vec!["/b".into(), "/a".into()]),
            updated_mcp_tool_output: Some(json!({"first": true})),
            retry: Some(true),
            ..HookResult::default()
        };
        let second = HookResult {
            additional_context: Some("second ctx".into()),
            system_message: Some("system two".into()),
            worktree_path: Some("/wt/two".into()),
            watch_paths: Some(vec!["/a".into(), "/c".into()]),
            updated_mcp_tool_output: Some(json!({"second": true})),
            ..HookResult::default()
        };

        let aggregated = aggregate_hook_results(&[first, second]);

        assert_eq!(
            aggregated.additional_contexts,
            vec!["first ctx", "second ctx"]
        );
        assert_eq!(aggregated.watch_paths, vec!["/a", "/b", "/c"]);
        assert_eq!(
            aggregated.updated_mcp_tool_output,
            Some(json!({"second": true}))
        );
        assert_eq!(aggregated.retry, Some(true));
        assert_eq!(aggregated.system_message.as_deref(), Some("system one"));
        assert_eq!(aggregated.worktree_path.as_deref(), Some("/wt/one"));
    }

    #[test]
    fn aggregate_hook_results_ask_beats_allow_and_keeps_ask_updated_input() {
        let allow = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            hook_permission_decision_reason: Some("allow".into()),
            updated_input: Some(serde_json::from_value(json!({"mode": "allow"})).unwrap()),
            ..HookResult::default()
        };
        let ask = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Ask),
            hook_permission_decision_reason: Some("ask".into()),
            updated_input: Some(serde_json::from_value(json!({"mode": "ask"})).unwrap()),
            ..HookResult::default()
        };

        let aggregated = aggregate_hook_results(&[allow, ask]);

        assert_eq!(
            aggregated.permission_behavior,
            Some(HookPermissionBehavior::Ask)
        );
        assert_eq!(
            aggregated.hook_permission_decision_reason.as_deref(),
            Some("ask")
        );
        assert_eq!(
            aggregated.updated_input.unwrap().get("mode"),
            Some(&json!("ask"))
        );
    }

    #[test]
    fn aggregate_hook_results_passthrough_beats_defer_but_loses_to_allow() {
        let defer = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Defer),
            hook_permission_decision_reason: Some("defer".into()),
            ..HookResult::default()
        };
        let passthrough = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Passthrough),
            hook_permission_decision_reason: Some("pass".into()),
            ..HookResult::default()
        };
        let allow = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            hook_permission_decision_reason: Some("allow".into()),
            ..HookResult::default()
        };

        let aggregated = aggregate_hook_results(&[defer, passthrough, allow]);

        assert_eq!(
            aggregated.permission_behavior,
            Some(HookPermissionBehavior::Allow)
        );
        assert_eq!(
            aggregated.hook_permission_decision_reason.as_deref(),
            Some("allow")
        );
    }

    #[test]
    fn aggregate_hook_results_updated_input_without_permission_is_preserved() {
        let result = HookResult {
            updated_input: Some(serde_json::from_value(json!({"patch": true})).unwrap()),
            ..HookResult::default()
        };

        let aggregated = aggregate_hook_results(&[result]);

        assert_eq!(
            aggregated.updated_input.unwrap().get("patch"),
            Some(&json!(true))
        );
    }

    #[test]
    fn aggregate_hook_results_deny_clears_updated_input_from_weaker_decisions() {
        let allow = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            updated_input: Some(serde_json::from_value(json!({"patch": true})).unwrap()),
            ..HookResult::default()
        };
        let deny = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Deny),
            hook_permission_decision_reason: Some("deny".into()),
            ..HookResult::default()
        };

        let aggregated = aggregate_hook_results(&[allow, deny]);

        assert_eq!(
            aggregated.permission_behavior,
            Some(HookPermissionBehavior::Deny)
        );
        assert!(aggregated.updated_input.is_none());
    }

    #[test]
    fn classify_plain_command_result_blocking_without_stderr_uses_default_message() {
        let result = classify_plain_command_result(2, "cmd", "");

        assert_eq!(
            result.blocking_error.unwrap().blocking_error,
            "[cmd]: No stderr output"
        );
    }

    #[test]
    fn classify_plain_command_result_nonzero_nonblocking_has_no_blocking_error() {
        let result = classify_plain_command_result(1, "cmd", "warning");

        assert_eq!(result.outcome, HookResultOutcome::NonBlockingError);
        assert!(result.blocking_error.is_none());
    }

    #[test]
    fn aggregate_hook_results_first_session_title_wins() {
        let first = HookResult {
            session_title: Some("first".into()),
            ..HookResult::default()
        };
        let second = HookResult {
            session_title: Some("second".into()),
            ..HookResult::default()
        };
        let aggregated = aggregate_hook_results(&[first, second]);
        assert_eq!(aggregated.session_title.as_deref(), Some("first"));
    }

    #[test]
    fn aggregate_hook_results_defer_loses_to_allow() {
        let allow = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            hook_permission_decision_reason: Some("allow".into()),
            ..HookResult::default()
        };
        let defer = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Defer),
            hook_permission_decision_reason: Some("defer".into()),
            ..HookResult::default()
        };
        let aggregated = aggregate_hook_results(&[defer, allow]);
        assert_eq!(
            aggregated.permission_behavior,
            Some(HookPermissionBehavior::Allow)
        );
        assert_eq!(
            aggregated.hook_permission_decision_reason.as_deref(),
            Some("allow")
        );
    }

    #[test]
    fn aggregate_hook_results_defer_wins_when_alone() {
        let defer = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Defer),
            hook_permission_decision_reason: Some("defer".into()),
            ..HookResult::default()
        };
        let aggregated = aggregate_hook_results(&[defer]);
        assert_eq!(
            aggregated.permission_behavior,
            Some(HookPermissionBehavior::Defer)
        );
    }

    #[test]
    fn aggregate_hook_results_uses_deny_ask_allow_precedence() {
        let allow = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            hook_permission_decision_reason: Some("allow".into()),
            outcome: HookResultOutcome::Success,
            ..HookResult::default()
        };
        let ask = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Ask),
            hook_permission_decision_reason: Some("ask".into()),
            outcome: HookResultOutcome::Success,
            ..HookResult::default()
        };
        let deny = HookResult {
            permission_behavior: Some(HookPermissionBehavior::Deny),
            hook_permission_decision_reason: Some("deny".into()),
            outcome: HookResultOutcome::Success,
            ..HookResult::default()
        };

        let aggregated = aggregate_hook_results(&[allow, ask, deny]);
        assert_eq!(
            aggregated.permission_behavior,
            Some(HookPermissionBehavior::Deny)
        );
        assert_eq!(
            aggregated.hook_permission_decision_reason.as_deref(),
            Some("deny")
        );
    }
}
