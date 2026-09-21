//! Host-facing effects produced by the hook runtime.
//!
//! An [`AggregatedHookResult`](crate::runtime_result::AggregatedHookResult)
//! is the union of everything the hooks said about one firing. It is not
//! the thing a host acts on: a host acts on a list of [`HookEffect`]s,
//! one per consequence it has to apply.
//!
//! Keeping aggregation and projection apart keeps both halves pure.
//! [`crate::runtime_result::aggregate_hook_results`] merges per-hook
//! results and does nothing else; [`project_effects`] decides which of
//! those merges become host actions for a given event, and a host then
//! picks one match arm per effect instead of threading twenty `Option`s
//! through its call sites.
//!
//! ## Ordering
//!
//! Effects come out in a fixed order — session-level decisions, then
//! permission decisions on tool-scoped events, then the rest — and the
//! tests below assert positions as well as membership. Reordering the
//! emission blocks in [`project_effects`] changes observable behaviour.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::HookEvent;
use crate::output_protocol::{HookPermissionBehavior, JsonObject, PermissionRequestResult};
use crate::runtime_result::{AggregatedHookResult, HookBlockingError};

/// One consequence the host must apply after running hooks for an
/// event. Projected from [`AggregatedHookResult`] by
/// [`project_effects`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookEffect {
    /// Block the originating tool call with the given reason.
    /// Produced when any hook returned `Deny` permission or a
    /// `decision: "block"` on a tool-scoped event.
    BlockToolCall {
        reason: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocking_errors: Vec<HookBlockingError>,
    },
    /// Downgrade an auto-allowed tool call to an interactive
    /// permission prompt. Carries the optional input patch the hook
    /// applied.
    AskPermission {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<JsonObject>,
    },
    /// Short-circuit the permission flow and allow the tool call.
    /// Carries the optional input patch.
    AllowToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<JsonObject>,
    },
    /// Patch an in-flight PermissionRequest with the hook's answer.
    /// Distinct from `AllowToolCall` / `BlockToolCall` because this
    /// one carries the wire-level `PermissionRequestResult` shape
    /// (including `updatedPermissions`) that the host needs to send
    /// back over ACP.
    PermissionRequestDecision { decision: PermissionRequestResult },
    /// Inject an `<additional_context>` block ahead of the next model
    /// turn. One effect per contributing hook; the runtime keeps
    /// their original order.
    InjectContext { text: String },
    /// Hook-produced tool output patch — replaces the MCP tool
    /// response before it lands in the message stream.
    UpdateToolOutput { output: Value },
    /// Patch the tool input going into the engine for the tool call
    /// the hook just inspected, when no permission decision was
    /// attached.
    UpdateToolInput { input: JsonObject },
    /// Set / override the session title shown in the host UI.
    /// Produced by `UserPromptSubmit.sessionTitle`.
    SetSessionTitle { title: String },
    /// Mark the current session as completed in host UI metadata.
    MarkSessionComplete,
    /// Seed the next user turn with this text. Produced by
    /// `SessionStart.initialUserMessage`.
    SeedInitialUserMessage { text: String },
    /// Continue a persistent goal in a fresh session. Produced by
    /// `SessionEnd.goalCompleted=false`.
    ContinueGoal {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Retry a previously denied tool call. Produced by
    /// `PermissionDenied.retry == true`.
    RetryDeniedTool,
    /// Stop the model loop for this session with the given reason.
    /// Produced by top-level `continue: false` or by `Stop` hook
    /// decisions.
    PreventContinuation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Block a session-level `Stop`. Lets the model keep running.
    /// Produced by `Stop` hooks that set `decision: "block"`.
    BlockStop { reason: String },
    /// Update the filesystem watcher's observed paths. Produced by
    /// `SessionStart` / `CwdChanged` / `FileChanged`.
    UpdateWatchPaths { paths: Vec<String> },
    /// Adopt the hook-created worktree at this path. Produced by
    /// `WorktreeCreate.worktreePath`.
    WorktreePath { path: String },
    /// Surface a system-level message to the UI (toast / status
    /// line). Produced by `systemMessage`.
    SystemMessage { text: String },
}

/// Turn an aggregated hook result into a list of effects for the
/// host to apply. The event is passed so per-event projections
/// (e.g. `Stop` vs `PreToolUse`) can branch cleanly; any effect that
/// doesn't apply to `event` is simply not emitted.
pub fn project_effects(event: HookEvent, agg: &AggregatedHookResult) -> Vec<HookEffect> {
    let mut effects = Vec::new();

    // --- Session-level decisions (highest priority) ---------------
    if agg.prevent_continuation == Some(true) {
        effects.push(HookEffect::PreventContinuation {
            reason: agg.stop_reason.clone(),
        });
    }

    // --- Permission decisions on tool-scoped events ---------------
    if matches!(
        event,
        HookEvent::PreToolUse | HookEvent::PermissionRequest | HookEvent::PermissionDenied
    ) {
        if let Some(behavior) = agg.permission_behavior {
            match behavior {
                HookPermissionBehavior::Deny => {
                    let reason = agg
                        .hook_permission_decision_reason
                        .clone()
                        .or_else(|| {
                            agg.blocking_errors
                                .first()
                                .map(|e| e.blocking_error.clone())
                        })
                        .unwrap_or_else(|| "Blocked by hook".into());
                    effects.push(HookEffect::BlockToolCall {
                        reason,
                        blocking_errors: agg.blocking_errors.clone(),
                    });
                }
                HookPermissionBehavior::Ask => {
                    effects.push(HookEffect::AskPermission {
                        reason: agg.hook_permission_decision_reason.clone(),
                        updated_input: agg.updated_input.clone(),
                    });
                }
                HookPermissionBehavior::Allow => {
                    effects.push(HookEffect::AllowToolCall {
                        updated_input: agg.updated_input.clone(),
                    });
                }
                HookPermissionBehavior::Passthrough | HookPermissionBehavior::Defer => {
                    // No decision — fall through to default host flow.
                }
            }
        }
    }

    if event == HookEvent::PermissionRequest {
        if let Some(decision) = &agg.permission_request_result {
            effects.push(HookEffect::PermissionRequestDecision {
                decision: decision.clone(),
            });
        }
    }

    // --- Blocking errors on non-tool events -----------------------
    //
    // A permission behavior is consumed above, and only on the three
    // tool-scoped events. Everywhere else a JSON `decision: "block"` gets
    // here with `Deny` still attached (`process_hook_json_output` sets
    // both), and it has to block the way an exit code 2 does: a
    // UserPromptSubmit hook answering `{"decision":"block"}` is the
    // documented way to refuse a prompt. Surface these separately so the
    // host can pick the right response for the event.
    let permission_consumed = matches!(
        event,
        HookEvent::PreToolUse | HookEvent::PermissionRequest | HookEvent::PermissionDenied
    ) && agg.permission_behavior.is_some();
    if !permission_consumed && !agg.blocking_errors.is_empty() {
        let reason = agg
            .blocking_errors
            .first()
            .map(|e| e.blocking_error.clone())
            .unwrap_or_else(|| "Blocked by hook".into());
        match event {
            HookEvent::Stop | HookEvent::SubagentStop => {
                effects.push(HookEffect::BlockStop { reason });
            }
            HookEvent::UserPromptSubmit
            | HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::PostToolUseFailure
            | HookEvent::PreCompact
            | HookEvent::ConfigChange => {
                effects.push(HookEffect::BlockToolCall {
                    reason,
                    blocking_errors: agg.blocking_errors.clone(),
                });
            }
            _ => {}
        }
    }

    // --- SessionEnd goal continuation -----------------------------
    if event == HookEvent::SessionEnd && agg.goal_completed == Some(false) {
        if let Some(prompt) = &agg.goal_continuation_prompt {
            if !prompt.trim().is_empty() {
                effects.push(HookEffect::ContinueGoal {
                    prompt: prompt.clone(),
                    reason: agg.goal_reason.clone(),
                });
            }
        }
    }

    // --- Retry of previously denied tool call ---------------------
    if event == HookEvent::PermissionDenied && agg.retry == Some(true) {
        effects.push(HookEffect::RetryDeniedTool);
    }

    // --- Context injection (order-preserving) ---------------------
    for context in &agg.additional_contexts {
        if !context.is_empty() {
            effects.push(HookEffect::InjectContext {
                text: context.clone(),
            });
        }
    }

    // --- UserPromptSubmit session title ---------------------------
    if let Some(title) = &agg.session_title {
        effects.push(HookEffect::SetSessionTitle {
            title: title.clone(),
        });
    }

    // --- SessionEnd completion marker ------------------------------
    if event == HookEvent::SessionEnd && agg.goal_completed == Some(true) {
        effects.push(HookEffect::MarkSessionComplete);
    }

    // --- SessionStart seed message --------------------------------
    if let Some(seed) = &agg.initial_user_message {
        effects.push(HookEffect::SeedInitialUserMessage { text: seed.clone() });
    }

    // --- Watch-path updates ---------------------------------------
    if !agg.watch_paths.is_empty() {
        effects.push(HookEffect::UpdateWatchPaths {
            paths: agg.watch_paths.clone(),
        });
    }

    // --- Worktree path adoption -----------------------------------
    if event == HookEvent::WorktreeCreate {
        if let Some(path) = &agg.worktree_path {
            effects.push(HookEffect::WorktreePath { path: path.clone() });
        }
    }

    // --- PostToolUse output patch ---------------------------------
    if let Some(output) = &agg.updated_mcp_tool_output {
        effects.push(HookEffect::UpdateToolOutput {
            output: output.clone(),
        });
    }

    // --- Standalone tool_input patch (no permission decision) -----
    if agg.permission_behavior.is_none() {
        if let Some(updated) = &agg.updated_input {
            effects.push(HookEffect::UpdateToolInput {
                input: updated.clone(),
            });
        }
    }

    // --- System messages ------------------------------------------
    if let Some(message) = &agg.system_message {
        if !message.is_empty() {
            effects.push(HookEffect::SystemMessage {
                text: message.clone(),
            });
        }
    }

    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_protocol::PermissionRequestResult;
    use serde_json::json;

    #[test]
    fn deny_produces_block_tool_call() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Deny),
            hook_permission_decision_reason: Some("nope".into()),
            blocking_errors: vec![HookBlockingError {
                blocking_error: "[cmd]: nope".into(),
                command: "cmd".into(),
            }],
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PreToolUse, &agg);
        assert!(matches!(
            effects.as_slice(),
            [HookEffect::BlockToolCall { reason, .. }] if reason == "nope"
        ));
    }

    #[test]
    fn ask_produces_ask_permission() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Ask),
            hook_permission_decision_reason: Some("reason".into()),
            updated_input: Some(serde_json::from_value(json!({"k": 1})).unwrap()),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PreToolUse, &agg);
        assert!(matches!(
            &effects[..],
            [HookEffect::AskPermission {
                reason: Some(r), updated_input: Some(_),
            }] if r == "reason"
        ));
    }

    #[test]
    fn allow_produces_allow_tool_call() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            updated_input: Some(serde_json::from_value(json!({"x": 1})).unwrap()),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PreToolUse, &agg);
        assert!(matches!(
            &effects[..],
            [HookEffect::AllowToolCall {
                updated_input: Some(_)
            }]
        ));
    }

    #[test]
    fn defer_projects_to_no_effect() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Defer),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PreToolUse, &agg);
        assert!(effects.is_empty());
    }

    #[test]
    fn prevent_continuation_is_first_effect() {
        let agg = AggregatedHookResult {
            prevent_continuation: Some(true),
            stop_reason: Some("halt".into()),
            permission_behavior: Some(HookPermissionBehavior::Allow),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PreToolUse, &agg);
        assert!(matches!(
            effects[0],
            HookEffect::PreventContinuation { reason: Some(ref r) } if r == "halt"
        ));
    }

    #[test]
    fn stop_event_block_becomes_block_stop() {
        let agg = AggregatedHookResult {
            blocking_errors: vec![HookBlockingError {
                blocking_error: "keep going".into(),
                command: "cmd".into(),
            }],
            ..Default::default()
        };
        let effects = project_effects(HookEvent::Stop, &agg);
        assert!(matches!(
            &effects[..],
            [HookEffect::BlockStop { reason }] if reason == "keep going"
        ));
    }

    #[test]
    fn json_block_decision_blocks_user_prompt_submit_and_stop() {
        // `{"decision":"block"}` arrives with Deny beside the blocking
        // error. Neither event consumes a permission behavior, so the
        // block must still come out — it used to vanish here.
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Deny),
            hook_permission_decision_reason: Some("not this".into()),
            blocking_errors: vec![HookBlockingError {
                blocking_error: "not this".into(),
                command: "cmd".into(),
            }],
            ..Default::default()
        };
        assert!(matches!(
            &project_effects(HookEvent::UserPromptSubmit, &agg)[..],
            [HookEffect::BlockToolCall { reason, .. }] if reason == "not this"
        ));
        assert!(matches!(
            &project_effects(HookEvent::Stop, &agg)[..],
            [HookEffect::BlockStop { reason }] if reason == "not this"
        ));
        // A tool-scoped event consumed the behavior above: one block, not two.
        assert!(matches!(
            &project_effects(HookEvent::PreToolUse, &agg)[..],
            [HookEffect::BlockToolCall { .. }]
        ));
    }

    #[test]
    fn user_prompt_submit_block_becomes_block_tool_call() {
        let agg = AggregatedHookResult {
            blocking_errors: vec![HookBlockingError {
                blocking_error: "reject".into(),
                command: "cmd".into(),
            }],
            ..Default::default()
        };
        let effects = project_effects(HookEvent::UserPromptSubmit, &agg);
        assert!(matches!(
            &effects[..],
            [HookEffect::BlockToolCall { reason, .. }] if reason == "reject"
        ));
    }

    #[test]
    fn permission_denied_retry_emits_retry_effect() {
        let agg = AggregatedHookResult {
            retry: Some(true),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PermissionDenied, &agg);
        assert!(effects.contains(&HookEffect::RetryDeniedTool));
    }

    #[test]
    fn additional_contexts_project_in_order() {
        let agg = AggregatedHookResult {
            additional_contexts: vec!["first".into(), "second".into(), "".into()],
            ..Default::default()
        };
        let effects = project_effects(HookEvent::UserPromptSubmit, &agg);
        let texts: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                HookEffect::InjectContext { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // Empty strings are skipped.
        assert_eq!(texts, vec!["first", "second"]);
    }

    #[test]
    fn session_title_projects() {
        let agg = AggregatedHookResult {
            session_title: Some("new-title".into()),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::UserPromptSubmit, &agg);
        assert!(effects.iter().any(|e| matches!(
            e,
            HookEffect::SetSessionTitle { title } if title == "new-title"
        )));
    }

    #[test]
    fn watch_paths_project() {
        let agg = AggregatedHookResult {
            watch_paths: vec!["/a".into(), "/b".into()],
            ..Default::default()
        };
        let effects = project_effects(HookEvent::CwdChanged, &agg);
        assert!(effects.contains(&HookEffect::UpdateWatchPaths {
            paths: vec!["/a".into(), "/b".into()]
        }));
    }

    #[test]
    fn session_end_incomplete_goal_projects_continuation() {
        let agg = AggregatedHookResult {
            goal_completed: Some(false),
            goal_continuation_prompt: Some("keep going".into()),
            goal_reason: Some("not done".into()),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::SessionEnd, &agg);
        assert!(matches!(
            &effects[..],
            [HookEffect::ContinueGoal { prompt, reason }] if prompt == "keep going" && reason.as_deref() == Some("not done")
        ));
    }

    #[test]
    fn session_end_complete_goal_projects_completion_marker_without_continuation() {
        let agg = AggregatedHookResult {
            goal_completed: Some(true),
            goal_continuation_prompt: Some("keep going".into()),
            goal_reason: Some("done".into()),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::SessionEnd, &agg);
        assert!(effects.contains(&HookEffect::MarkSessionComplete));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, HookEffect::ContinueGoal { .. })));
    }

    #[test]
    fn permission_request_decision_projects() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            permission_request_result: Some(PermissionRequestResult::Allow {
                updated_input: Some(serde_json::from_value(json!({"y": 2})).unwrap()),
                updated_permissions: None,
            }),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PermissionRequest, &agg);
        assert!(effects
            .iter()
            .any(|e| matches!(e, HookEffect::PermissionRequestDecision { .. })));
    }

    #[test]
    fn updated_input_without_permission_becomes_update_tool_input() {
        let agg = AggregatedHookResult {
            updated_input: Some(serde_json::from_value(json!({"z": 3})).unwrap()),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PreToolUse, &agg);
        assert!(effects
            .iter()
            .any(|e| matches!(e, HookEffect::UpdateToolInput { .. })));
    }

    #[test]
    fn permission_request_deny_projects_block_and_wire_decision() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Deny),
            hook_permission_decision_reason: Some("policy denied".into()),
            permission_request_result: Some(PermissionRequestResult::Deny {
                message: Some("no".into()),
                interrupt: Some(false),
            }),
            ..Default::default()
        };

        let effects = project_effects(HookEvent::PermissionRequest, &agg);

        assert!(effects.iter().any(|effect| matches!(
            effect,
            HookEffect::BlockToolCall { reason, .. } if reason == "policy denied"
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            HookEffect::PermissionRequestDecision {
                decision: PermissionRequestResult::Deny { .. }
            }
        )));
    }

    #[test]
    fn permission_behavior_is_ignored_on_non_permission_events() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Deny),
            hook_permission_decision_reason: Some("deny".into()),
            ..Default::default()
        };

        let effects = project_effects(HookEvent::SessionStart, &agg);

        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, HookEffect::BlockToolCall { .. })));
    }

    #[test]
    fn passthrough_projects_to_no_permission_effect() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Passthrough),
            ..Default::default()
        };
        let effects = project_effects(HookEvent::PreToolUse, &agg);
        assert!(effects.is_empty());
    }

    #[test]
    fn block_stop_also_applies_to_subagent_stop() {
        let agg = AggregatedHookResult {
            blocking_errors: vec![HookBlockingError {
                blocking_error: "keep subagent running".into(),
                command: "cmd".into(),
            }],
            ..Default::default()
        };

        let effects = project_effects(HookEvent::SubagentStop, &agg);

        assert!(matches!(
            &effects[..],
            [HookEffect::BlockStop { reason }] if reason == "keep subagent running"
        ));
    }

    #[test]
    fn pre_compact_and_config_change_blocks_project_as_block_tool_call() {
        for event in [
            HookEvent::PreCompact,
            HookEvent::ConfigChange,
            HookEvent::PostToolUse,
        ] {
            let agg = AggregatedHookResult {
                blocking_errors: vec![HookBlockingError {
                    blocking_error: "blocked".into(),
                    command: "cmd".into(),
                }],
                ..Default::default()
            };

            let effects = project_effects(event, &agg);

            assert!(effects
                .iter()
                .any(|effect| matches!(effect, HookEffect::BlockToolCall { reason, .. } if reason == "blocked")),
                "{event:?}: {effects:?}"
            );
        }
    }

    #[test]
    fn seed_initial_user_message_projects() {
        let agg = AggregatedHookResult {
            initial_user_message: Some("hello from setup".into()),
            ..Default::default()
        };

        let effects = project_effects(HookEvent::SessionStart, &agg);

        assert!(effects.contains(&HookEffect::SeedInitialUserMessage {
            text: "hello from setup".into()
        }));
    }

    #[test]
    fn update_tool_output_projects() {
        let agg = AggregatedHookResult {
            updated_mcp_tool_output: Some(json!({"patched": true})),
            ..Default::default()
        };

        let effects = project_effects(HookEvent::PostToolUse, &agg);

        assert!(effects.contains(&HookEffect::UpdateToolOutput {
            output: json!({"patched": true})
        }));
    }

    #[test]
    fn worktree_path_projects_only_for_worktree_create() {
        let agg = AggregatedHookResult {
            worktree_path: Some("/tmp/wt".into()),
            ..Default::default()
        };

        let create_effects = project_effects(HookEvent::WorktreeCreate, &agg);
        let setup_effects = project_effects(HookEvent::Setup, &agg);

        assert!(create_effects.contains(&HookEffect::WorktreePath {
            path: "/tmp/wt".into()
        }));
        assert!(!setup_effects
            .iter()
            .any(|effect| matches!(effect, HookEffect::WorktreePath { .. })));
    }

    #[test]
    fn system_message_projects_when_non_empty() {
        let agg = AggregatedHookResult {
            system_message: Some("status".into()),
            ..Default::default()
        };

        let effects = project_effects(HookEvent::Setup, &agg);

        assert!(effects.contains(&HookEffect::SystemMessage {
            text: "status".into()
        }));
    }

    #[test]
    fn empty_system_message_is_suppressed() {
        let agg = AggregatedHookResult {
            system_message: Some(String::new()),
            ..Default::default()
        };

        let effects = project_effects(HookEvent::Setup, &agg);

        assert!(effects.is_empty());
    }

    #[test]
    fn updated_input_with_permission_decision_does_not_emit_standalone_update() {
        let agg = AggregatedHookResult {
            permission_behavior: Some(HookPermissionBehavior::Allow),
            updated_input: Some(serde_json::from_value(json!({"x": 1})).unwrap()),
            ..Default::default()
        };

        let effects = project_effects(HookEvent::PreToolUse, &agg);

        assert!(effects
            .iter()
            .any(|effect| matches!(effect, HookEffect::AllowToolCall { .. })));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, HookEffect::UpdateToolInput { .. })));
    }
}
