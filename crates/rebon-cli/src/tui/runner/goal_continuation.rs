use std::sync::Arc;

use async_trait::async_trait;
use rebon_api::{
    ContentBlock as ApiContentBlock, CreateMessageRequest, Message as ApiMessage, ModelClient,
    Role, TextBlock,
};
use rebon_hooks::output_protocol::{HookJsonOutput, HookSpecificOutput, SyncHookJsonOutput};
use rebon_hooks::{
    ExecutedHookResult, HookEffect, HookEventPayload, HookExecutionError, HookExecutor,
    HookInvocationInput, HookRuntimeContext,
};

use crate::goal::{build_goal_continuation_prompt, parse_goal_check_decision};
use rebon_session::format_system_time_iso_ms;
use tokio::runtime::Handle;

use crate::session::submit_payload::SubmitPayload;
use crate::tui::app::{AppState, PromptCompletionStatus};
use crate::tui::runner::commands::apply_new_session;
use crate::tui::runner::title::mark_session_title_completed;
use crate::tui::runner::transcript_messages::inject_system_message;
use crate::tui::wiring::TuiEngineSession;

const TRANSCRIPT_SUMMARY_LIMIT: usize = 16_000;
const GOAL_CHECK_MAX_TOKENS: u32 = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GoalContinuationAction {
    None,
    Continue {
        prompt: String,
        reason: Option<String>,
    },
    Completed {
        reason: Option<String>,
    },
    Paused,
    Error {
        message: String,
    },
}

pub(super) fn maybe_prepare_goal_continuation(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    stop_reason: Option<String>,
) -> GoalContinuationAction {
    let Some(goal) = app.goal.as_ref() else {
        return GoalContinuationAction::None;
    };
    if !goal.is_active() {
        return GoalContinuationAction::None;
    }

    let goal_snapshot = goal.clone();
    let old_session_id = session.session_id.clone();
    let transcript_summary = transcript_summary(app);
    let previous_session_context = if transcript_summary.trim().is_empty() {
        None
    } else {
        Some(transcript_summary.clone())
    };
    let action = handle.block_on(run_goal_session_end_hook(
        goal_snapshot,
        session,
        &old_session_id,
        stop_reason,
        transcript_summary,
    ));

    let action = match action {
        Ok(action) => action,
        Err(message) => GoalContinuationAction::Error { message },
    };

    match &action {
        GoalContinuationAction::Continue { prompt, reason } => {
            if !app.goal.as_ref().is_some_and(|goal| goal.can_continue()) {
                let message = app
                    .goal
                    .as_mut()
                    .map(|goal| {
                        goal.mark_paused();
                        let max_sessions = goal.max_sessions.expect("continuation limit reached");
                        format!(
                            "Goal paused after reaching the max session limit ({}/{}): {}",
                            goal.sessions_started, max_sessions, goal.prompt
                        )
                    })
                    .unwrap_or_else(|| "Goal paused: no active goal.".to_string());
                app.deferred_goal_submit_payloads.clear();
                inject_system_message(app, "warning", &message);
                return GoalContinuationAction::Paused;
            }
            if let Some(goal) = app.goal.as_mut() {
                goal.mark_continued();
            }
            let continuing_goal = app.goal.clone();
            apply_new_session(app, session);
            app.goal = continuing_goal;
            let continuation_prompt = app
                .goal
                .as_ref()
                .map(|goal| {
                    build_goal_continuation_prompt(
                        &goal.prompt,
                        previous_session_context.as_deref(),
                        Some(prompt.as_str()),
                        reason.as_deref(),
                    )
                })
                .unwrap_or_else(|| prompt.clone());
            app.deferred_goal_submit_payloads.push(SubmitPayload {
                text: prompt.clone(),
                model_text: Some(continuation_prompt),
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            });
        }
        GoalContinuationAction::Completed { reason } => {
            if let Some(goal) = app.goal.as_mut() {
                goal.mark_complete(reason.clone());
            }
            mark_session_title_completed(app, session, handle);
            let content = reason
                .as_ref()
                .map(|reason| format!("Goal complete: {reason}"))
                .unwrap_or_else(|| "Goal complete.".to_string());
            inject_system_message(
                app,
                "info",
                &format!("{content}\nGoal is complete and stopped. The goal remains available for review or /goal archive."),
            );
        }
        GoalContinuationAction::Paused => {}
        GoalContinuationAction::Error { message } => {
            app.prompt_completion_status = Some(PromptCompletionStatus::Failed);
            inject_system_message(app, "error", message);
        }
        GoalContinuationAction::None => {}
    }

    action
}

fn goal_session_end_snapshot(
    hook: rebon_hooks::IndividualHookConfig,
) -> rebon_hooks::SettingsSnapshot {
    rebon_hooks::SettingsSnapshot {
        session_hooks: vec![hook],
        ..Default::default()
    }
}

async fn run_goal_session_end_hook(
    goal: crate::goal::GoalState,
    session: &TuiEngineSession,
    _old_session_id: &str,
    stop_reason: Option<String>,
    transcript_summary: String,
) -> Result<GoalContinuationAction, String> {
    let hook = rebon_hooks::IndividualHookConfig {
        event: rebon_hooks::HookEvent::SessionEnd,
        config: rebon_hooks::HookCommand::Prompt(rebon_hooks::PromptHook {
            prompt: crate::goal::build_goal_check_prompt(&goal.prompt, &transcript_summary),
            r#if: None,
            timeout: Some(120),
            model: Some(session.model.name.clone()),
            status_message: Some("Checking goal".into()),
            once: None,
        }),
        matcher: None,
        source: rebon_hooks::HookSource::SessionHook,
        plugin_name: None,
    };
    let snapshot = goal_session_end_snapshot(hook);
    let provider = Arc::new(rebon_hooks::SettingsHookProvider::new(snapshot));
    let metadata = Arc::new(rebon_hooks::build_hook_event_metadata(
        &rebon_hooks::MetadataInputs::default(),
    ));
    let executor = Arc::new(
        rebon_hooks::DispatchExecutor::new()
            .with_command(Box::new(rebon_hooks::CommandExecutor::new()))
            .with_http(Box::new(rebon_hooks::HttpExecutor::new()))
            .with_prompt(Box::new(GoalPromptExecutor {
                client: session.engine_half.client.clone(),
                model: session.model.name.clone(),
            })),
    );
    // Not a policy event: this is the goal feature using the hook runtime as
    // an executor for a prompt it wrote itself, and it needs the whole
    // `HookRuntimeOutput` — `goal_completed`, the execution and validation
    // errors — which a verdict does not carry. It borrows the session's
    // invocation context so both paths agree on where the session is.
    let runtime = rebon_hooks::HookRuntime::new(provider, metadata, executor);
    let input = HookInvocationInput::new(
        session.engine_half.runtime.policy.context().clone(),
        HookEventPayload::SessionEnd {
            reason: stop_reason,
        },
    );
    let output = runtime.run_event(&input).await;
    for effect in output.effects {
        match effect {
            HookEffect::ContinueGoal { prompt, reason } => {
                return Ok(GoalContinuationAction::Continue { prompt, reason });
            }
            HookEffect::SystemMessage { text } => {
                tracing::info!(message = %text, "goal hook message");
            }
            _ => {}
        }
    }
    if output.aggregated.goal_completed == Some(true) {
        return Ok(GoalContinuationAction::Completed {
            reason: output.aggregated.goal_reason,
        });
    }
    if let Some(err) = output
        .execution_errors
        .first()
        .map(|err| err.error.clone())
        .or_else(|| {
            output
                .validation_errors
                .first()
                .map(|err| err.error.clone())
        })
    {
        return Err(format!("Goal hook failed: {err}"));
    }
    Ok(GoalContinuationAction::None)
}

struct GoalPromptExecutor {
    client: Arc<dyn ModelClient>,
    model: String,
}

#[async_trait]
impl HookExecutor for GoalPromptExecutor {
    async fn execute(
        &self,
        hook: &rebon_hooks::IndividualHookConfig,
        _input: &HookInvocationInput,
        _ctx: &HookRuntimeContext,
    ) -> Result<ExecutedHookResult, HookExecutionError> {
        let prompt = match &hook.config {
            rebon_hooks::HookCommand::Prompt(prompt) => prompt,
            other => return Err(HookExecutionError::UnsupportedType(other.type_str().into())),
        };
        let request = CreateMessageRequest {
            model: prompt.model.clone().unwrap_or_else(|| self.model.clone()),
            messages: vec![ApiMessage {
                role: Role::User,
                content: vec![ApiContentBlock::Text(TextBlock {
                    text: prompt.prompt.clone(),
                })],
            }],
            system: Some(crate::goal::DEFAULT_GOAL_CHECK_SYSTEM_PROMPT.to_string()),
            max_tokens: GOAL_CHECK_MAX_TOKENS,
            stream: false,
            ..CreateMessageRequest::simple("", "")
        };
        let message =
            self.client.create_message(request).await.map_err(|err| {
                HookExecutionError::Transport(format!("goal check failed: {err}"))
            })?;
        let text = message.text();
        let decision = parse_goal_check_decision(&text).map_err(|err| {
            HookExecutionError::Transport(format!("goal check returned invalid output: {err}"))
        })?;
        let continuation_prompt = if decision.completed {
            None
        } else {
            Some(decision.next_prompt.clone().unwrap_or_else(|| {
                decision
                    .reason
                    .as_ref()
                    .map(|reason| format!("Continue the goal; unresolved work: {reason}"))
                    .unwrap_or_else(|| "Continue the goal and verify what remains.".to_string())
            }))
        };
        Ok(ExecutedHookResult::json(
            HookJsonOutput::Sync(SyncHookJsonOutput {
                system_message: Some(if decision.completed {
                    decision
                        .reason
                        .as_ref()
                        .map(|reason| format!("Goal complete: {reason}"))
                        .unwrap_or_else(|| "Goal complete.".to_string())
                } else {
                    decision
                        .reason
                        .as_ref()
                        .map(|reason| format!("Goal continues: {reason}"))
                        .unwrap_or_else(|| "Goal continues.".to_string())
                }),
                hook_specific_output: Some(HookSpecificOutput::SessionEnd {
                    goal_completed: Some(decision.completed),
                    continuation_prompt,
                    reason: decision.reason,
                }),
                ..SyncHookJsonOutput::default()
            }),
            "goal-check",
        ))
    }
}

fn transcript_summary(app: &AppState) -> String {
    let mut out = String::new();
    for row in app.rebon_tui.transcript.rows().iter().rev() {
        let entry = summarize_row(row);
        if entry.trim().is_empty() {
            continue;
        }
        if out.len() + entry.len() + 2 > TRANSCRIPT_SUMMARY_LIMIT {
            break;
        }
        if !out.is_empty() {
            out.insert_str(0, "\n\n");
        }
        out.insert_str(0, &entry);
    }
    out
}

fn summarize_row(row: &rebon_tui::Message) -> String {
    match row {
        rebon_tui::Message::User(message) => {
            format!("User: {}", user_text(&message.message.content))
        }
        rebon_tui::Message::Assistant(message) => {
            format!("Assistant: {}", assistant_text(&message.message.content))
        }
        rebon_tui::Message::System(message) => {
            format!("System: {}", message.content.clone().unwrap_or_default())
        }
        rebon_tui::Message::Attachment(_) | rebon_tui::Message::Unknown => String::new(),
    }
}

fn user_text(blocks: &[rebon_tui::UserContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            rebon_tui::UserContentBlock::Text(text) => Some(text.text.clone()),
            rebon_tui::UserContentBlock::ToolResult(result) => Some(format!(
                "Tool result: {}",
                result.content.as_display_string()
            )),
            rebon_tui::UserContentBlock::Image(_) => Some("[image]".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_text(blocks: &[rebon_tui::AssistantContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            rebon_tui::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            rebon_tui::AssistantContentBlock::Thinking(_) => Some("[thinking]".to_string()),
            rebon_tui::AssistantContentBlock::RedactedThinking(_) => {
                Some("[redacted thinking]".to_string())
            }
            rebon_tui::AssistantContentBlock::ToolUse(tool) => {
                Some(format!("Tool use {}: {}", tool.name, tool.input))
            }
            rebon_tui::AssistantContentBlock::GeneratedImage(_) => {
                Some("[generated image]".to_string())
            }
            rebon_tui::AssistantContentBlock::Other => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn commit_goal_continuation_feedback(
    app: &mut AppState,
    action: &GoalContinuationAction,
) {
    let Some(goal) = app.goal.as_ref() else {
        return;
    };
    let GoalContinuationAction::Continue { reason, .. } = action else {
        return;
    };
    let reason = reason
        .as_ref()
        .map(|reason| format!(" Reason: {reason}"))
        .unwrap_or_default();
    let text = if let Some(max_sessions) = goal.max_sessions {
        format!(
            "Goal not complete; starting continuation session {}/{}.{}",
            goal.sessions_started, max_sessions, reason
        )
    } else {
        format!(
            "Goal not complete; starting continuation session {}.{}",
            goal.sessions_started, reason
        )
    };
    let uuid = format!("s-goal-{}", rebon_types::wall_clock_ms_u128());
    let timestamp = format_system_time_iso_ms(std::time::SystemTime::now());
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::Commit(rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid,
            timestamp,
            subtype: "goal".into(),
            content: Some(text),
            level: Some(rebon_tui::SystemLevel::Info),
            is_meta: None,
        })),
    );
    app.follow_transcript_tail = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{StreamEvent, Usage};

    fn goal_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: "goal-check".into(),
                model: "test-model".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: rebon_api::ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: rebon_api::ContentBlockDelta::TextDelta { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageStop,
        ]
    }

    fn make_goal_session_with_mock(
        mock: rebon_api::MockModelClient,
    ) -> (tempfile::TempDir, crate::tui::wiring::TuiEngineSession) {
        let mut session = super::super::test_support::make_test_tui_session();
        let client: Arc<dyn ModelClient> = Arc::new(mock);
        session.engine_half.client = client.clone();
        session.model.runtime_model =
            rebon_core::query::SharedRuntimeModel::new(rebon_core::query::RuntimeModelConfig {
                provider_name: "test".into(),
                client,
                model: "test-model".into(),
                model_profiles: rebon_types::ModelProfileMap::default(),
                title_model: "test-small-model".into(),
                model_marketing_name: None,
                knowledge_cutoff: None,
                prune_level: Some(rebon_api::PruneLevelHandle::new(rebon_api::PruneLevel::Off)),
                compact_provider: None,
                compact_fallback_provider: None,
                context_management: None,
                reasoning_mode: None,
            });
        let projects_root = tempfile::Builder::new()
            .prefix("rebon-goal-continuation-test-")
            .tempdir()
            .unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        (projects_root, session)
    }

    fn commit_user_text(app: &mut AppState, text: &str) {
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::User(rebon_tui::UserMessage {
                uuid: format!("u-{text}"),
                timestamp: "t".into(),
                message: rebon_tui::UserMessageInner {
                    role: rebon_tui::UserRole::User,
                    content: vec![rebon_tui::UserContentBlock::Text(
                        rebon_tui::UserTextBlock { text: text.into() },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            })),
        );
    }

    fn commit_assistant_text(app: &mut AppState, text: &str) {
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: format!("a-{text}"),
                timestamp: "t".into(),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::Text(
                        rebon_tui::AssistantTextBlock { text: text.into() },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );
    }
    #[test]
    fn goal_session_end_snapshot_contains_only_session_hook() {
        let hook = rebon_hooks::IndividualHookConfig {
            event: rebon_hooks::HookEvent::SessionEnd,
            config: rebon_hooks::HookCommand::Prompt(rebon_hooks::PromptHook {
                prompt: "check goal".into(),
                r#if: None,
                timeout: None,
                model: None,
                status_message: None,
                once: None,
            }),
            matcher: None,
            source: rebon_hooks::HookSource::SessionHook,
            plugin_name: None,
        };

        let snapshot = goal_session_end_snapshot(hook);

        assert!(snapshot.settings_hooks.is_empty());
        assert!(snapshot.plugin_hooks.is_empty());
        assert_eq!(snapshot.session_hooks.len(), 1);
        assert_eq!(
            snapshot.session_hooks[0].source,
            rebon_hooks::HookSource::SessionHook
        );
    }

    #[test]
    fn automatic_goal_continuation_preserves_goal_across_fresh_session() {
        let mock = rebon_api::MockModelClient::new();
        mock.push_turn(goal_reply(
            r#"{"completed":false,"reason":"tests remain","nextPrompt":"run tests"}"#,
        ));
        let handle = tokio::runtime::Runtime::new().expect("runtime");
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_now("ship it").with_max_sessions(3));
        commit_user_text(&mut app, "implement parser");
        commit_assistant_text(&mut app, "cargo test failed in parser_test");
        let (_projects_root, mut session) = make_goal_session_with_mock(mock);
        let old_session_id = session.session_id.clone();

        let action = maybe_prepare_goal_continuation(
            &mut app,
            &mut session,
            handle.handle(),
            Some("end_turn".into()),
        );

        assert!(matches!(
            action,
            GoalContinuationAction::Continue { ref prompt, .. } if prompt == "run tests"
        ));
        assert_ne!(session.session_id, old_session_id);
        let goal = app.goal.as_ref().expect("goal preserved");
        assert_eq!(goal.prompt, "ship it");
        assert_eq!(goal.sessions_started, 2);
        assert_eq!(goal.max_sessions, Some(3));
        assert_eq!(app.deferred_goal_submit_payloads.len(), 1);
        let submit = &app.deferred_goal_submit_payloads[0];
        assert_eq!(submit.text, "run tests");
        let continuation_prompt = submit.model_text.as_deref().expect("model goal prompt");
        assert!(continuation_prompt.contains("<goal>\nship it\n</goal>"));
        assert!(continuation_prompt.contains("Previous session context"));
        assert!(continuation_prompt.contains("User: implement parser"));
        assert!(continuation_prompt.contains("Assistant: cargo test failed in parser_test"));
        assert!(continuation_prompt.contains("Next requested step:\nrun tests"));
        assert!(continuation_prompt.contains("tests remain"));
        assert!(continuation_prompt.contains("Completion audit before stopping"));
    }

    #[test]
    fn completed_goal_stops_without_clearing() {
        let mock = rebon_api::MockModelClient::new();
        mock.push_turn(goal_reply(r#"{"completed":true,"reason":"tests pass"}"#));
        let handle = tokio::runtime::Runtime::new().expect("runtime");
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_now("ship it").with_max_sessions(3));
        let (_projects_root, mut session) = make_goal_session_with_mock(mock);
        let old_session_id = session.session_id.clone();

        let action = maybe_prepare_goal_continuation(
            &mut app,
            &mut session,
            handle.handle(),
            Some("end_turn".into()),
        );

        assert!(matches!(
            action,
            GoalContinuationAction::Completed { ref reason } if reason.as_deref() == Some("tests pass")
        ));
        assert_eq!(session.session_id, old_session_id);
        let goal = app.goal.as_ref().expect("goal preserved after completion");
        assert!(goal.is_complete());
        assert_eq!(goal.completed_reason.as_deref(), Some("tests pass"));
        assert!(app.deferred_goal_submit_payloads.is_empty());
    }

    #[test]
    fn goal_continuation_fallback_uses_short_visible_next_step() {
        let mock = rebon_api::MockModelClient::new();
        mock.push_turn(goal_reply(
            r#"{"completed":false,"reason":"tests still need to run"}"#,
        ));
        let handle = tokio::runtime::Runtime::new().expect("runtime");
        let mut app = AppState::new();
        app.goal = Some(crate::goal::GoalState::new_now("ship it").with_max_sessions(3));
        let (_projects_root, mut session) = make_goal_session_with_mock(mock);

        let action = maybe_prepare_goal_continuation(
            &mut app,
            &mut session,
            handle.handle(),
            Some("end_turn".into()),
        );

        assert!(matches!(
            action,
            GoalContinuationAction::Continue { ref prompt, .. }
                if prompt == "Continue the goal; unresolved work: tests still need to run"
        ));
        let submit = &app.deferred_goal_submit_payloads[0];
        assert_eq!(
            submit.text,
            "Continue the goal; unresolved work: tests still need to run"
        );
        let continuation_prompt = submit.model_text.as_deref().expect("model goal prompt");
        assert!(continuation_prompt.contains("<goal>\nship it\n</goal>"));
        assert_eq!(continuation_prompt.matches("<goal>").count(), 1);
        assert!(continuation_prompt.contains(
            "Next requested step:\nContinue the goal; unresolved work: tests still need to run"
        ));
        assert!(continuation_prompt.contains("tests still need to run"));
    }

    #[test]
    fn paused_goal_does_not_continue() {
        let mock = rebon_api::MockModelClient::new();
        let handle = tokio::runtime::Runtime::new().expect("runtime");
        let mut app = AppState::new();
        let mut goal = crate::goal::GoalState::new_now("ship it").with_max_sessions(3);
        goal.mark_paused();
        app.goal = Some(goal);
        let (_projects_root, mut session) = make_goal_session_with_mock(mock);
        let old_session_id = session.session_id.clone();

        let action = maybe_prepare_goal_continuation(
            &mut app,
            &mut session,
            handle.handle(),
            Some("end_turn".into()),
        );

        assert_eq!(action, GoalContinuationAction::None);
        assert_eq!(session.session_id, old_session_id);
        assert!(app.deferred_goal_submit_payloads.is_empty());
        assert!(app.deferred_internal_submit_payloads.is_empty());
    }

    #[test]
    fn transcript_summary_includes_user_and_assistant_text() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::User(rebon_tui::UserMessage {
                uuid: "u".into(),
                timestamp: "t".into(),
                message: rebon_tui::UserMessageInner {
                    role: rebon_tui::UserRole::User,
                    content: vec![rebon_tui::UserContentBlock::Text(
                        rebon_tui::UserTextBlock {
                            text: "do the thing".into(),
                        },
                    )],
                },
                is_compact_summary: None,
                is_meta: None,
                is_visible_in_transcript_only: None,
                image_paste_ids: None,
                plan_content: None,
            })),
        );
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: "a".into(),
                timestamp: "t".into(),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::Text(
                        rebon_tui::AssistantTextBlock {
                            text: "done".into(),
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );

        let summary = transcript_summary(&app);
        assert!(summary.contains("User: do the thing"));
        assert!(summary.contains("Assistant: done"));
    }
}
