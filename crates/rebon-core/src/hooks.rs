use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rebon_hooks::{
    build_hook_event_metadata, load_all_editable, CommandExecutor, DispatchExecutor, HookEffect,
    HookEventPayload, HookInvocationContext, HookInvocationInput, HookRuntime, HookRuntimeOutput,
    HttpExecutor, IndividualHookConfig, MetadataInputs, SettingsHookProvider, SettingsPaths,
    SettingsSnapshot,
};
use rebon_tool::{PermissionBroker, Tool, ToolContext};
use rebon_tools_core::{PermissionDecision, ToolError, ToolResult};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::policy_seat::{PolicyFuture, PolicyRequest, PolicySources, PolicySubscriber, Verdict};

/// The hooks a user configured, as one subscriber on the policy seat.
///
/// This is the first subscriber and, until a plugin registers one, the only
/// one. It is intentionally small and immutable per firing: settings hooks
/// are loaded from the editable settings paths for the cwd **the request
/// names**, then composed with the plugin hooks this session loaded.
/// Command and HTTP transports are wired here. Prompt/agent hooks require
/// model and worker-spawner executors owned by higher layers, so a caller
/// that needs those transports builds a specialised runtime around
/// [`run_snapshot_event`] instead of registering here.
///
/// It carries no cwd, transcript path, or session id of its own. Those come
/// off [`PolicyRequest::context`], which the emit handle stamps, so a
/// session that rebinds to another directory cannot leave a subscriber
/// answering about the old one.
#[derive(Debug, Clone, Default)]
pub struct SettingsHookSubscriber {
    metadata_inputs: MetadataInputs,
    plugin_hooks: Vec<IndividualHookConfig>,
}

#[derive(Debug, Error)]
pub enum ProductHookRuntimeError {
    #[error("failed to load hook settings: {0}")]
    LoadSettings(#[from] rebon_hooks::SettingsLoadError),
}

impl SettingsHookSubscriber {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_metadata_inputs(mut self, metadata_inputs: MetadataInputs) -> Self {
        self.metadata_inputs = metadata_inputs;
        self
    }

    pub fn with_plugin_hooks(mut self, plugin_hooks: Vec<IndividualHookConfig>) -> Self {
        self.plugin_hooks = plugin_hooks;
        self
    }

    pub fn load_settings_snapshot(
        &self,
        cwd: &Path,
    ) -> Result<SettingsSnapshot, ProductHookRuntimeError> {
        // The user settings file lives in the config home, resolved the way
        // the rest of the process resolves it.
        let paths = SettingsPaths::from_config_dir(&rebon_session::default_config_home_dir(), cwd);
        let loaded = load_all_editable(&paths)?;
        if !loaded.warnings.is_empty() {
            for warning in &loaded.warnings {
                tracing::warn!(warning = ?warning, "hook settings load warning");
            }
        }
        Ok(SettingsSnapshot {
            settings_hooks: loaded.hooks,
            plugin_hooks: self.plugin_hooks.clone(),
            session_hooks: Vec::new(),
            ..Default::default()
        })
    }

    async fn run(
        &self,
        request: &PolicyRequest,
    ) -> Result<HookRuntimeOutput, ProductHookRuntimeError> {
        let snapshot = self.load_settings_snapshot(Path::new(&request.context.cwd))?;
        Ok(run_snapshot_event(
            snapshot,
            request.context.clone(),
            request.payload.clone(),
            &self.metadata_inputs,
        )
        .await)
    }
}

impl PolicySubscriber for SettingsHookSubscriber {
    fn decide<'a>(&'a self, request: &'a PolicyRequest) -> PolicyFuture<'a> {
        Box::pin(async move {
            match self.run(request).await {
                Ok(output) => Verdict::Modify {
                    effects: output.effects,
                },
                // Settings that will not load is the one failure this
                // subscriber has, and it is not a policy answer: the hooks
                // never ran, so there is nothing for them to have decided.
                // Warning and standing aside is what every emit site did
                // with this error before the seat existed.
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        event = %request.kind().name(),
                        "hook settings failed to load; the configured hooks did not run"
                    );
                    Verdict::Allow
                }
            }
        })
    }
}

pub async fn run_snapshot_event(
    snapshot: SettingsSnapshot,
    context: HookInvocationContext,
    payload: HookEventPayload,
    metadata_inputs: &MetadataInputs,
) -> HookRuntimeOutput {
    let provider = Arc::new(SettingsHookProvider::new(snapshot));
    let metadata = Arc::new(build_hook_event_metadata(metadata_inputs));
    let executor = Arc::new(
        DispatchExecutor::new()
            .with_command(Box::new(CommandExecutor::new()))
            .with_http(Box::new(HttpExecutor::new())),
    );
    let runtime = HookRuntime::new(provider, metadata, executor);
    let input = HookInvocationInput::new(context, payload);
    runtime.run_event(&input).await
}

#[derive(Debug, Clone, PartialEq)]
pub enum PreToolUseDecision {
    Continue { input: Value },
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionHookDecision {
    Continue { input: Value },
    Allow { input: Value },
    Deny { reason: String },
}

pub struct HookedPermissionBroker {
    inner: Arc<dyn PermissionBroker>,
    policy: PolicySources,
}

impl HookedPermissionBroker {
    pub fn new(inner: Arc<dyn PermissionBroker>, policy: PolicySources) -> Self {
        Self { inner, policy }
    }

    pub fn inner(&self) -> &Arc<dyn PermissionBroker> {
        &self.inner
    }
}

#[async_trait]
impl PermissionBroker for HookedPermissionBroker {
    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        let tool_name = tool.id().as_str().to_string();
        let tool_use_id = context.tool_use_id().unwrap_or_default().to_string();
        let mut effective_input = input;
        let mut effective_decision = decision;

        match run_permission_request_hooks(
            &self.policy,
            &tool_name,
            effective_input.clone(),
            &tool_use_id,
            effective_decision.reason.clone().or_else(|| {
                effective_decision
                    .request
                    .as_ref()
                    .map(|request| request.message.clone())
            }),
        )
        .await
        {
            PermissionHookDecision::Continue { input } => {
                effective_input = input;
            }
            PermissionHookDecision::Allow { input } => {
                effective_input = input.clone();
                effective_decision = PermissionDecision::allow(input);
            }
            PermissionHookDecision::Deny { reason } => {
                run_permission_denied_hooks(
                    &self.policy,
                    &tool_name,
                    effective_input.clone(),
                    &tool_use_id,
                    Some(reason.clone()),
                )
                .await;
                return Err(ToolError::PermissionDenied {
                    tool: tool.id(),
                    reason,
                });
            }
        }

        let result = self
            .inner
            .resolve(tool, effective_input.clone(), context, effective_decision)
            .await;
        if let Err(ToolError::PermissionDenied { reason, .. }) = &result {
            run_permission_denied_hooks(
                &self.policy,
                &tool_name,
                effective_input,
                &tool_use_id,
                Some(reason.clone()),
            )
            .await;
        }
        result
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub async fn run_pre_tool_use_hooks(
    policy: &PolicySources,
    tool_name: &str,
    tool_input: Value,
    tool_use_id: &str,
) -> PreToolUseDecision {
    let verdict = policy
        .emit(HookEventPayload::PreToolUse {
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            tool_use_id: tool_use_id.to_string(),
        })
        .await;
    // A gated event's terminal refusal — a subscriber that said no, or one
    // that never answered — is this event's block.
    if let Some(reason) = verdict.denial() {
        return PreToolUseDecision::Blocked {
            reason: reason.to_string(),
        };
    }
    apply_pre_tool_use_effects(tool_input, verdict.effects())
}

pub fn apply_pre_tool_use_effects(input: Value, effects: &[HookEffect]) -> PreToolUseDecision {
    let mut effective_input = input;
    for effect in effects {
        match effect {
            HookEffect::BlockToolCall { reason, .. } => {
                return PreToolUseDecision::Blocked {
                    reason: reason.clone(),
                };
            }
            HookEffect::AllowToolCall {
                updated_input: Some(updated),
            }
            | HookEffect::AskPermission {
                updated_input: Some(updated),
                ..
            }
            | HookEffect::UpdateToolInput { input: updated } => {
                effective_input = Value::Object(updated.clone());
            }
            HookEffect::SystemMessage { text } => {
                tracing::info!(message = %text, "pre-tool-use hook message");
            }
            unsupported => {
                tracing::debug!(effect = ?unsupported, "unsupported PreToolUse hook effect ignored");
            }
        }
    }
    PreToolUseDecision::Continue {
        input: effective_input,
    }
}

pub async fn run_post_tool_use_hooks(
    policy: &PolicySources,
    tool_name: &str,
    tool_input: Value,
    tool_response: Value,
    tool_use_id: &str,
) -> Value {
    let verdict = policy
        .emit(HookEventPayload::PostToolUse {
            tool_name: tool_name.to_string(),
            tool_input,
            tool_response: tool_response.clone(),
            tool_use_id: tool_use_id.to_string(),
        })
        .await;
    apply_post_tool_use_effects(tool_response, verdict.effects())
}

pub async fn run_post_tool_use_failure_hooks(
    policy: &PolicySources,
    tool_name: &str,
    tool_input: Value,
    tool_use_id: &str,
    error: &str,
) {
    let verdict = policy
        .emit(HookEventPayload::PostToolUseFailure {
            tool_name: tool_name.to_string(),
            tool_input,
            tool_use_id: tool_use_id.to_string(),
            error: error.to_string(),
        })
        .await;
    for effect in verdict.effects() {
        match effect {
            HookEffect::SystemMessage { text } => {
                tracing::info!(message = %text, "post-tool-use-failure hook message");
            }
            unsupported => tracing::debug!(
                effect = ?unsupported,
                "unsupported PostToolUseFailure hook effect ignored: tool errors are already surfaced as strings at this seam"
            ),
        }
    }
}

pub fn apply_post_tool_use_effects(output: Value, effects: &[HookEffect]) -> Value {
    let mut effective_output = output;
    for effect in effects {
        match effect {
            HookEffect::UpdateToolOutput { output } => {
                effective_output = output.clone();
            }
            HookEffect::SystemMessage { text } => {
                tracing::info!(message = %text, "post-tool-use hook message");
            }
            unsupported => {
                tracing::debug!(effect = ?unsupported, "unsupported PostToolUse hook effect ignored")
            }
        }
    }
    effective_output
}

pub async fn run_permission_request_hooks(
    policy: &PolicySources,
    tool_name: &str,
    tool_input: Value,
    tool_use_id: &str,
    reason: Option<String>,
) -> PermissionHookDecision {
    let input_object = tool_input.as_object().cloned().unwrap_or_else(Map::new);
    let verdict = policy
        .emit(HookEventPayload::PermissionRequest {
            tool_name: tool_name.to_string(),
            tool_input: input_object,
            tool_use_id: tool_use_id.to_string(),
            reason,
        })
        .await;
    // Gated: a terminal refusal is this event's denial, and the caller
    // still runs the PermissionDenied event over it.
    if let Some(reason) = verdict.denial() {
        return PermissionHookDecision::Deny {
            reason: reason.to_string(),
        };
    }
    apply_permission_request_effects(tool_input, verdict.effects())
}

pub async fn run_permission_denied_hooks(
    policy: &PolicySources,
    tool_name: &str,
    tool_input: Value,
    tool_use_id: &str,
    message: Option<String>,
) {
    let input_object = tool_input.as_object().cloned().unwrap_or_else(Map::new);
    let verdict = policy
        .emit(HookEventPayload::PermissionDenied {
            tool_name: tool_name.to_string(),
            tool_input: input_object,
            tool_use_id: tool_use_id.to_string(),
            message,
        })
        .await;
    for effect in verdict.effects() {
        match effect {
            HookEffect::SystemMessage { text } => {
                tracing::info!(message = %text, "permission-denied hook message");
            }
            HookEffect::RetryDeniedTool => tracing::debug!(
                "unsupported PermissionDenied RetryDeniedTool effect ignored: retry scheduling is owned by the TUI denial replay flow"
            ),
            unsupported => tracing::debug!(effect = ?unsupported, "unsupported PermissionDenied hook effect ignored"),
        }
    }
}

pub fn apply_permission_request_effects(
    input: Value,
    effects: &[HookEffect],
) -> PermissionHookDecision {
    let mut effective_input = input;
    let mut allow = false;
    for effect in effects {
        match effect {
            HookEffect::PermissionRequestDecision { decision } => match decision {
                rebon_hooks::output_protocol::PermissionRequestResult::Allow {
                    updated_input,
                    ..
                } => {
                    allow = true;
                    if let Some(updated) = updated_input {
                        effective_input = Value::Object(updated.clone());
                    }
                }
                rebon_hooks::output_protocol::PermissionRequestResult::Deny { message, .. } => {
                    return PermissionHookDecision::Deny {
                        reason: message
                            .clone()
                            .unwrap_or_else(|| "Permission denied by hook".to_string()),
                    };
                }
            },
            HookEffect::AllowToolCall { updated_input } => {
                allow = true;
                if let Some(updated) = updated_input {
                    effective_input = Value::Object(updated.clone());
                }
            }
            HookEffect::BlockToolCall { reason, .. } => {
                return PermissionHookDecision::Deny {
                    reason: reason.clone(),
                };
            }
            HookEffect::UpdateToolInput { input } => {
                effective_input = Value::Object(input.clone());
            }
            HookEffect::SystemMessage { text } => {
                tracing::info!(message = %text, "permission-request hook message");
            }
            unsupported => tracing::debug!(
                effect = ?unsupported,
                "unsupported PermissionRequest hook effect ignored"
            ),
        }
    }
    if allow {
        PermissionHookDecision::Allow {
            input: effective_input,
        }
    } else {
        PermissionHookDecision::Continue {
            input: effective_input,
        }
    }
}

/// The messages a session-lifecycle event wants shown, out of its
/// effects. One projection for both `SessionStart` and `SessionEnd`, in
/// the engine, so the two hosts that run them cannot drift on what a
/// lifecycle hook may say.
pub fn apply_session_lifecycle_effects(effects: &[HookEffect]) -> Vec<String> {
    let mut system_messages = Vec::new();
    for effect in effects {
        match effect {
            HookEffect::SystemMessage { text }
            | HookEffect::InjectContext { text }
            | HookEffect::SeedInitialUserMessage { text } => {
                system_messages.push(text.clone());
            }
            HookEffect::UpdateWatchPaths { paths } => tracing::debug!(
                paths = ?paths,
                "unsupported session lifecycle UpdateWatchPaths effect ignored: product file watcher path updates are not exposed here yet"
            ),
            unsupported => tracing::debug!(
                effect = ?unsupported,
                "unsupported session lifecycle hook effect ignored"
            ),
        }
    }
    system_messages
}

pub fn apply_stop_effects(effects: &[HookEffect]) -> Result<(), String> {
    for effect in effects {
        match effect {
            HookEffect::BlockStop { reason } => return Err(reason.clone()),
            HookEffect::SystemMessage { text } => {
                tracing::info!(message = %text, "stop hook message")
            }
            unsupported => {
                tracing::debug!(effect = ?unsupported, "unsupported Stop hook effect ignored")
            }
        }
    }
    Ok(())
}

/// What a `UserPromptSubmit` hook run decided about the prompt.
///
/// One shape for both hosts that run the event: the TUI before it admits a
/// local turn, and a worker before it runs a claimed one. The effects are
/// projected here so the two cannot drift on what a hook can do to a
/// prompt; what each host *shows* for a message or a refusal stays its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserPromptSubmitDecision {
    /// The prompt does not run. The reason goes where the prompt was typed.
    Blocked { reason: String },
    /// The prompt runs, with these applied first.
    Continue(UserPromptSubmitEffects),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserPromptSubmitEffects {
    /// `<additional_context>` blocks to append to the prompt, in hook order.
    /// See [`append_additional_context`] for the one way they are attached.
    pub additional_context: Vec<String>,
    /// Messages for the transcript, not for the model.
    pub system_messages: Vec<String>,
    /// `UserPromptSubmit.sessionTitle`, last one wins.
    pub session_title: Option<String>,
    pub mark_session_complete: bool,
}

pub fn apply_user_prompt_submit_effects(effects: &[HookEffect]) -> UserPromptSubmitDecision {
    let mut applied = UserPromptSubmitEffects::default();
    for effect in effects {
        match effect {
            // A block ends it: nothing a later hook said about a prompt that
            // is not going to run is worth applying.
            HookEffect::BlockToolCall { reason, .. } => {
                return UserPromptSubmitDecision::Blocked {
                    reason: reason.clone(),
                };
            }
            HookEffect::InjectContext { text } => applied.additional_context.push(text.clone()),
            HookEffect::SystemMessage { text } => applied.system_messages.push(text.clone()),
            HookEffect::SetSessionTitle { title } => applied.session_title = Some(title.clone()),
            HookEffect::MarkSessionComplete => applied.mark_session_complete = true,
            unsupported => tracing::debug!(
                effect = ?unsupported,
                "unsupported UserPromptSubmit hook effect ignored"
            ),
        }
    }
    UserPromptSubmitDecision::Continue(applied)
}

/// Attach one hook-provided context block to the prompt text, the way
/// every host does it — so a hook author sees the same thing reach the
/// model whether the turn ran in the terminal or in a worker.
pub fn append_additional_context(prompt: &mut String, text: &str) {
    if !prompt.is_empty() {
        prompt.push_str("\n\n");
    }
    prompt.push_str("<additional_context>\n");
    prompt.push_str(text);
    prompt.push_str("\n</additional_context>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_hooks::{BashCommandHook, HookCommand, HookEvent, HookSource};
    use serde_json::json;

    #[test]
    fn pre_tool_use_block_effect_prevents_tool() {
        let effects = vec![HookEffect::BlockToolCall {
            reason: "blocked".into(),
            blocking_errors: Vec::new(),
        }];
        assert_eq!(
            apply_pre_tool_use_effects(json!({"command":"echo hi"}), &effects),
            PreToolUseDecision::Blocked {
                reason: "blocked".into()
            }
        );
    }

    #[tokio::test]
    async fn settings_snapshot_session_end_hook_executes() {
        let hook = IndividualHookConfig {
            event: HookEvent::SessionEnd,
            config: HookCommand::Command(BashCommandHook {
                command: "process.stderr.write('ended'); process.exit(2)".into(),
                r#if: None,
                shell: Some(rebon_hooks::ShellKind::Node),
                timeout: Some(5),
                status_message: None,
                once: None,
                r#async: None,
                async_rewake: None,
            }),
            matcher: Some("stop".into()),
            source: HookSource::UserSettings,
            plugin_name: None,
        };
        let output = run_snapshot_event(
            SettingsSnapshot {
                settings_hooks: vec![hook],
                ..Default::default()
            },
            HookInvocationContext {
                cwd: std::env::current_dir()
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                session_id: "s".into(),
                ..Default::default()
            },
            HookEventPayload::SessionEnd {
                reason: Some("stop".into()),
            },
            &MetadataInputs::default(),
        )
        .await;
        assert_eq!(
            output.selected, 1,
            "effects={:?} per_hook={:?}",
            output.effects, output.per_hook
        );
        assert!(
            output.execution_errors.is_empty(),
            "{:?}",
            output.execution_errors
        );
        assert!(
            output.validation_errors.is_empty(),
            "{:?}",
            output.validation_errors
        );
        assert!(
            output
                .per_hook
                .iter()
                .any(|result| result.outcome == rebon_hooks::HookResultOutcome::Blocking),
            "{:?}",
            output.per_hook
        );
    }

    #[tokio::test]
    async fn pre_tool_use_decision_blocks_from_runtime_output() {
        let output = rebon_hooks::HookRuntimeOutput {
            selected: 1,
            per_hook: Vec::new(),
            aggregated: rebon_hooks::AggregatedHookResult::default(),
            effects: vec![HookEffect::BlockToolCall {
                reason: "no bash".into(),
                blocking_errors: Vec::new(),
            }],
            execution_errors: Vec::new(),
            validation_errors: Vec::new(),
        };
        assert_eq!(
            apply_pre_tool_use_effects(json!({"command":"rm -rf /"}), &output.effects),
            PreToolUseDecision::Blocked {
                reason: "no bash".into()
            }
        );
    }

    #[test]
    fn pre_tool_use_updates_input_from_allow_effect() {
        let effects = vec![HookEffect::AllowToolCall {
            updated_input: Some(serde_json::from_value(json!({"command":"echo safe"})).unwrap()),
        }];
        assert_eq!(
            apply_pre_tool_use_effects(json!({"command":"echo unsafe"}), &effects),
            PreToolUseDecision::Continue {
                input: json!({"command":"echo safe"})
            }
        );
    }

    #[test]
    fn post_tool_use_update_output_effect_replaces_result() {
        let effects = vec![HookEffect::UpdateToolOutput {
            output: json!({"ok": true, "rewritten": 1}),
        }];
        assert_eq!(
            apply_post_tool_use_effects(json!({"ok": false}), &effects),
            json!({"ok": true, "rewritten": 1})
        );
    }

    #[test]
    fn permission_request_decision_allow_updates_input() {
        let effects = vec![HookEffect::PermissionRequestDecision {
            decision: rebon_hooks::output_protocol::PermissionRequestResult::Allow {
                updated_input: Some(serde_json::from_value(json!({"path":"safe"})).unwrap()),
                updated_permissions: None,
            },
        }];
        assert_eq!(
            apply_permission_request_effects(json!({"path":"unsafe"}), &effects),
            PermissionHookDecision::Allow {
                input: json!({"path":"safe"})
            }
        );
    }

    #[test]
    fn stop_block_effect_returns_reason() {
        let effects = vec![HookEffect::BlockStop {
            reason: "keep running".into(),
        }];
        assert_eq!(apply_stop_effects(&effects), Err("keep running".into()));
    }
    #[test]
    fn user_prompt_submit_block_wins_over_everything_after_and_before_it() {
        let effects = vec![
            HookEffect::InjectContext {
                text: "before".into(),
            },
            HookEffect::SetSessionTitle {
                title: "named".into(),
            },
            HookEffect::BlockToolCall {
                reason: "not today".into(),
                blocking_errors: Vec::new(),
            },
            HookEffect::InjectContext {
                text: "after".into(),
            },
        ];
        assert_eq!(
            apply_user_prompt_submit_effects(&effects),
            UserPromptSubmitDecision::Blocked {
                reason: "not today".into()
            }
        );
    }

    #[test]
    fn user_prompt_submit_effects_keep_hook_order_and_last_title() {
        let effects = vec![
            HookEffect::InjectContext {
                text: "first".into(),
            },
            HookEffect::SystemMessage {
                text: "shown".into(),
            },
            HookEffect::SetSessionTitle {
                title: "one".into(),
            },
            HookEffect::InjectContext {
                text: "second".into(),
            },
            HookEffect::SetSessionTitle {
                title: "two".into(),
            },
            HookEffect::MarkSessionComplete,
            // Not a prompt effect; ignored rather than refused.
            HookEffect::BlockStop {
                reason: "irrelevant".into(),
            },
        ];
        assert_eq!(
            apply_user_prompt_submit_effects(&effects),
            UserPromptSubmitDecision::Continue(UserPromptSubmitEffects {
                additional_context: vec!["first".into(), "second".into()],
                system_messages: vec!["shown".into()],
                session_title: Some("two".into()),
                mark_session_complete: true,
            })
        );
    }

    #[test]
    fn user_prompt_submit_without_hooks_changes_nothing() {
        assert_eq!(
            apply_user_prompt_submit_effects(&[]),
            UserPromptSubmitDecision::Continue(UserPromptSubmitEffects::default())
        );
    }

    #[test]
    fn additional_context_is_attached_the_same_way_everywhere() {
        let mut prompt = String::from("do the thing");
        append_additional_context(&mut prompt, "ctx one");
        append_additional_context(&mut prompt, "ctx two");
        assert_eq!(
            prompt,
            "do the thing\n\n<additional_context>\nctx one\n</additional_context>\n\n<additional_context>\nctx two\n</additional_context>"
        );
        let mut empty = String::new();
        append_additional_context(&mut empty, "only");
        assert_eq!(empty, "<additional_context>\nonly\n</additional_context>");
    }
    /// A settings file, a real subscriber on a real handle, a real
    /// subprocess. `REBON_CONFIG_DIR` points at an empty directory so the
    /// developer's own hooks cannot join in.
    struct HookSettings {
        _guard: std::sync::MutexGuard<'static, ()>,
        _config_home: tempfile::TempDir,
        project: tempfile::TempDir,
        old_config_dir: Option<String>,
    }

    impl HookSettings {
        fn with(settings: Value) -> Self {
            let guard = crate::test_env_lock();
            let config_home = tempfile::tempdir().expect("config home");
            let project = tempfile::tempdir().expect("project dir");
            let dot_rebon = project.path().join(".rebon");
            std::fs::create_dir_all(&dot_rebon).expect("project .rebon");
            std::fs::write(
                dot_rebon.join("settings.json"),
                serde_json::to_string_pretty(&settings).expect("settings json"),
            )
            .expect("write settings");
            let old_config_dir = std::env::var("REBON_CONFIG_DIR").ok();
            std::env::set_var("REBON_CONFIG_DIR", config_home.path());
            Self {
                _guard: guard,
                _config_home: config_home,
                project,
                old_config_dir,
            }
        }

        /// The handle a session would build for this project directory.
        fn policy(&self) -> PolicySources {
            PolicySources::default()
                .with_context(HookInvocationContext {
                    cwd: self.project.path().to_string_lossy().to_string(),
                    session_id: "e2e-session".into(),
                    ..Default::default()
                })
                .with_subscriber(
                    crate::policy_seat::SETTINGS_HOOKS_SUBSCRIBER_ID,
                    crate::turn_hook::Order::NORMAL,
                    Arc::new(SettingsHookSubscriber::new()),
                )
        }
    }

    impl Drop for HookSettings {
        fn drop(&mut self) {
            match self.old_config_dir.as_deref() {
                Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
        }
    }

    fn pre_tool_use_hook(matcher: &str, command: &str) -> Value {
        json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": matcher,
                    "hooks": [{
                        "type": "command",
                        "command": command,
                        "shell": "node",
                    }],
                }],
            }
        })
    }

    /// The whole chain a user gets when they write a `PreToolUse` hook:
    /// settings file, seat, subscriber, subprocess, exit code 2, refusal.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_hook_that_exits_two_blocks_the_tool_through_the_seat() {
        let settings = HookSettings::with(pre_tool_use_hook(
            "Bash",
            "process.stderr.write('no bash in this project'); process.exit(2)",
        ));

        let decision = run_pre_tool_use_hooks(
            &settings.policy(),
            "Bash",
            json!({"command": "echo hi"}),
            "tool-1",
        )
        .await;

        match decision {
            PreToolUseDecision::Blocked { reason } => {
                assert!(reason.contains("no bash in this project"), "{reason}");
            }
            other => panic!("exit code 2 must refuse the call: {other:?}"),
        }
    }

    /// The same settings, a tool the matcher does not name: the hook does
    /// not run and the input reaches the tool untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hook_whose_matcher_does_not_name_the_tool_leaves_it_alone() {
        let settings = HookSettings::with(pre_tool_use_hook(
            "Bash",
            "process.stderr.write('should not run'); process.exit(2)",
        ));

        let decision = run_pre_tool_use_hooks(
            &settings.policy(),
            "Read",
            json!({"path": "notes.md"}),
            "tool-1",
        )
        .await;

        assert_eq!(
            decision,
            PreToolUseDecision::Continue {
                input: json!({"path": "notes.md"})
            }
        );
    }

    /// A sub-agent's handle is the session's, tagged. The same configured
    /// hook has to reach it — this is what "sub-agents run hooks now" means.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sub_agents_tool_call_reaches_the_sessions_configured_hook() {
        let settings = HookSettings::with(pre_tool_use_hook(
            "Bash",
            "process.stderr.write('guarded everywhere'); process.exit(2)",
        ));

        let decision = run_pre_tool_use_hooks(
            &settings.policy().in_agent("explorer-1"),
            "Bash",
            json!({"command": "ls"}),
            "tool-1",
        )
        .await;

        assert!(
            matches!(decision, PreToolUseDecision::Blocked { .. }),
            "a delegated call must meet the same guard: {decision:?}"
        );
    }

    /// A hook that rewrites the input rather than refusing it, end to end.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_hook_can_rewrite_the_tool_input() {
        let allow = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "updatedInput": { "command": "echo safe" },
            }
        });
        let settings = HookSettings::with(pre_tool_use_hook(
            "Bash",
            &format!("process.stdout.write({})", json!(allow.to_string())),
        ));

        let decision = run_pre_tool_use_hooks(
            &settings.policy(),
            "Bash",
            json!({"command": "echo unsafe"}),
            "tool-1",
        )
        .await;

        assert_eq!(
            decision,
            PreToolUseDecision::Continue {
                input: json!({"command": "echo safe"})
            }
        );
    }
}
