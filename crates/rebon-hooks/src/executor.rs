//! Hook execution dispatch.
//!
//! This module owns the *protocol and orchestration* of hooks, not their
//! I/O. [`HookExecutor`] is the single seam the runtime calls to run a
//! hook; a caller registers one backend per transport:
//!
//! | Variant                | Backend                                                 |
//! |------------------------|---------------------------------------------------------|
//! | `HookCommand::Command` | Spawn a subprocess, stream stdin JSON, parse stdout.    |
//! | `HookCommand::Http`    | POST a JSON body, parse the response.                   |
//! | `HookCommand::Prompt`  | Run the prompt through a model loop as a one-shot turn. |
//! | `HookCommand::Agent`   | Spawn a worker agent.                                   |
//!
//! Only the command and HTTP backends ship here (see [`crate::executors`]);
//! prompt and agent backends need a model client or a worker spawner, so
//! the caller supplies them.
//!
//! Every backend returns an [`ExecutedHookResult`]: parsed JSON output,
//! plain-text tail, validation error, and exit code. The runtime hands
//! that to `runtime_result::process_hook_json_output` /
//! `classify_plain_command_result` to get a
//! [`crate::runtime_result::HookResult`].
//!
//! ## Why one trait
//!
//! The runtime has to run "the next hook" without knowing which transport
//! it is. It iterates `&[IndividualHookConfig]` and calls
//! `executor.execute(hook, input, ctx)`; the impl behind the trait picks
//! the backend. Without that single seam the runtime could not order or
//! deduplicate hooks across transports at all.

use async_trait::async_trait;
use std::time::Duration;
use thiserror::Error;

use crate::hook_command::HookCommand;
use crate::individual_hook::IndividualHookConfig;
use crate::invocation::HookInvocationInput;
use crate::output_protocol::HookJsonOutput;

/// Optional per-invocation context the runtime passes alongside the
/// input (distinct from [`HookInvocationInput`], which is the bytes
/// the hook actually sees). Carries host-level concerns that don't
/// belong in the stdin JSON: the hook's `matcher` string and its
/// timeout-override.
#[derive(Debug, Clone, Default)]
pub struct HookRuntimeContext {
    /// Matcher string that selected this hook. Used by test doubles
    /// and logging only — the executor should not branch on it.
    pub matcher: Option<String>,
    /// Override timeout if the host wants to enforce a ceiling below
    /// the hook's own `timeout` field. `None` means "use the hook's
    /// own timeout or the caller's default".
    pub timeout_override: Option<Duration>,
}

/// What a single hook produced, before projection into
/// [`HookResult`](crate::runtime_result::HookResult). Carries the
/// (stdout, stderr, exit_code) tuple a command hook produces plus the
/// parsed JSON if the stdout was valid.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutedHookResult {
    /// Parsed sync / async JSON output, when the hook produced valid
    /// JSON. `None` for plain-text or validation-error outputs.
    pub json: Option<HookJsonOutput>,
    /// Plain-text output, when the hook wrote non-JSON to stdout.
    pub plain_text: Option<String>,
    /// Validation error, when the output parsed as JSON but didn't
    /// match the expected schema.
    pub validation_error: Option<String>,
    /// Exit code (command) or HTTP status (http). For prompt/agent
    /// hooks this is synthesised — 0 on success, 1 otherwise.
    pub exit_code: i32,
    /// Raw stderr, for blocking-error messages. Empty string when
    /// the transport has no stderr channel (http/prompt/agent).
    pub stderr: String,
    /// The command string the host will render in blocking-error
    /// messages. For command hooks it's the `command` field; for
    /// prompt/agent hooks it's the prompt; for http hooks it's the
    /// URL. Kept distinct from display rules so the runtime can
    /// build a uniform error label.
    pub command_label: String,
}

impl ExecutedHookResult {
    /// Mark a hook that completed successfully but produced no JSON
    /// output.
    pub fn plain(text: impl Into<String>, exit_code: i32, label: impl Into<String>) -> Self {
        Self {
            json: None,
            plain_text: Some(text.into()),
            validation_error: None,
            exit_code,
            stderr: String::new(),
            command_label: label.into(),
        }
    }

    /// Mark a hook that produced valid JSON output.
    pub fn json(json: HookJsonOutput, label: impl Into<String>) -> Self {
        Self {
            json: Some(json),
            plain_text: None,
            validation_error: None,
            exit_code: 0,
            stderr: String::new(),
            command_label: label.into(),
        }
    }
}

/// Error the executor may return. Distinct from a hook that ran to
/// completion and reported a failing exit code — that is a successful
/// execution with `exit_code != 0`.
#[derive(Debug, Error, Clone)]
pub enum HookExecutionError {
    #[error("hook timed out after {0:?}")]
    Timeout(Duration),
    #[error("hook was cancelled")]
    Cancelled,
    #[error("hook transport error: {0}")]
    Transport(String),
    #[error("hook handler unsupported for hook type '{0}'")]
    UnsupportedType(String),
    #[error("hook if-predicate evaluation failed: {0}")]
    IfEvaluationFailed(String),
}

/// Single seam the runtime calls to run a hook. Implementors pick
/// the transport (subprocess, HTTP, prompt, agent) based on
/// `hook.config`.
///
/// The runtime guarantees:
///
/// 1. `input.hook_event_name == hook.event` — the caller never
///    routes a mismatched event through.
/// 2. The `if`-predicate has already been evaluated, so the
///    executor does NOT re-filter.
/// 3. `ctx.timeout_override.is_some()` means "ceiling everything at
///    this value"; without it, respect `hook.config`'s own
///    `timeout` or a transport-default.
#[async_trait]
pub trait HookExecutor: Send + Sync {
    async fn execute(
        &self,
        hook: &IndividualHookConfig,
        input: &HookInvocationInput,
        ctx: &HookRuntimeContext,
    ) -> Result<ExecutedHookResult, HookExecutionError>;
}

/// A composite executor that dispatches by [`HookCommand`] variant.
/// Callers register one backend per variant; the runtime picks the
/// right backend per hook.
///
/// Left as a thin dispatcher so the host can override just one
/// transport (e.g. swap the HTTP backend for a mock) without
/// rewriting the others.
#[derive(Default)]
pub struct DispatchExecutor {
    pub command: Option<Box<dyn HookExecutor>>,
    pub prompt: Option<Box<dyn HookExecutor>>,
    pub agent: Option<Box<dyn HookExecutor>>,
    pub http: Option<Box<dyn HookExecutor>>,
}

impl DispatchExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_command(mut self, executor: Box<dyn HookExecutor>) -> Self {
        self.command = Some(executor);
        self
    }
    pub fn with_prompt(mut self, executor: Box<dyn HookExecutor>) -> Self {
        self.prompt = Some(executor);
        self
    }
    pub fn with_agent(mut self, executor: Box<dyn HookExecutor>) -> Self {
        self.agent = Some(executor);
        self
    }
    pub fn with_http(mut self, executor: Box<dyn HookExecutor>) -> Self {
        self.http = Some(executor);
        self
    }

    fn backend_for(&self, hook: &HookCommand) -> Option<&dyn HookExecutor> {
        match hook {
            HookCommand::Command(_) => self.command.as_deref(),
            HookCommand::Prompt(_) => self.prompt.as_deref(),
            HookCommand::Agent(_) => self.agent.as_deref(),
            HookCommand::Http(_) => self.http.as_deref(),
        }
    }
}

#[async_trait]
impl HookExecutor for DispatchExecutor {
    async fn execute(
        &self,
        hook: &IndividualHookConfig,
        input: &HookInvocationInput,
        ctx: &HookRuntimeContext,
    ) -> Result<ExecutedHookResult, HookExecutionError> {
        match self.backend_for(&hook.config) {
            Some(backend) => backend.execute(hook, input, ctx).await,
            None => Err(HookExecutionError::UnsupportedType(
                hook.config.type_str().to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookEvent;
    use crate::hook_command::{AgentHook, BashCommandHook, HookCommand, HttpHook, PromptHook};
    use crate::hook_source::HookSource;
    use crate::invocation::{HookEventPayload, HookInvocationContext};
    use crate::output_protocol::{HookSpecificOutput, SyncHookJsonOutput};
    use std::time::Duration;

    struct RecordingExecutor {
        label: String,
    }

    #[async_trait]
    impl HookExecutor for RecordingExecutor {
        async fn execute(
            &self,
            hook: &IndividualHookConfig,
            _input: &HookInvocationInput,
            _ctx: &HookRuntimeContext,
        ) -> Result<ExecutedHookResult, HookExecutionError> {
            Ok(ExecutedHookResult::plain(
                format!("{} ran {}", self.label, hook.config.type_str()),
                0,
                self.label.clone(),
            ))
        }
    }

    fn hook_with(cmd: HookCommand) -> IndividualHookConfig {
        IndividualHookConfig {
            event: HookEvent::PreToolUse,
            config: cmd,
            matcher: None,
            source: HookSource::UserSettings,
            plugin_name: None,
        }
    }

    fn invocation() -> HookInvocationInput {
        HookInvocationInput::new(
            HookInvocationContext::default(),
            HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            },
        )
    }

    #[tokio::test]
    async fn dispatch_executor_routes_by_variant() {
        let dispatcher = DispatchExecutor::new()
            .with_command(Box::new(RecordingExecutor {
                label: "CMD".into(),
            }))
            .with_http(Box::new(RecordingExecutor {
                label: "HTTP".into(),
            }));

        let cmd_hook = hook_with(HookCommand::Command(BashCommandHook {
            command: "ls".into(),
            r#if: None,
            shell: None,
            timeout: None,
            status_message: None,
            once: None,
            r#async: None,
            async_rewake: None,
        }));
        let http_hook = hook_with(HookCommand::Http(HttpHook {
            url: "https://x".into(),
            r#if: None,
            timeout: None,
            headers: None,
            allowed_env_vars: None,
            status_message: None,
            once: None,
        }));

        let input = invocation();
        let ctx = HookRuntimeContext::default();

        let cmd_result = dispatcher.execute(&cmd_hook, &input, &ctx).await.unwrap();
        assert_eq!(cmd_result.plain_text.as_deref(), Some("CMD ran command"));

        let http_result = dispatcher.execute(&http_hook, &input, &ctx).await.unwrap();
        assert_eq!(http_result.plain_text.as_deref(), Some("HTTP ran http"));
    }

    #[tokio::test]
    async fn dispatch_executor_missing_backend_reports_unsupported() {
        let dispatcher = DispatchExecutor::new();
        let hook = hook_with(HookCommand::Prompt(PromptHook {
            prompt: "hi".into(),
            r#if: None,
            timeout: None,
            model: None,
            status_message: None,
            once: None,
        }));
        let err = dispatcher
            .execute(&hook, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, HookExecutionError::UnsupportedType(ref t) if t == "prompt"));
    }

    #[tokio::test]
    async fn dispatch_executor_agent_backend_plugs_in() {
        let dispatcher = DispatchExecutor::new().with_agent(Box::new(RecordingExecutor {
            label: "AGENT".into(),
        }));
        let hook = hook_with(HookCommand::Agent(AgentHook {
            prompt: "verify".into(),
            r#if: None,
            timeout: None,
            model: None,
            status_message: None,
            once: None,
        }));
        let result = dispatcher
            .execute(&hook, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap();
        assert_eq!(result.plain_text.as_deref(), Some("AGENT ran agent"));
    }

    #[tokio::test]
    async fn dispatch_executor_routes_prompt_backend() {
        let dispatcher = DispatchExecutor::new().with_prompt(Box::new(RecordingExecutor {
            label: "PROMPT".into(),
        }));
        let hook = hook_with(HookCommand::Prompt(PromptHook {
            prompt: "summarize".into(),
            r#if: None,
            timeout: None,
            model: None,
            status_message: None,
            once: None,
        }));

        let result = dispatcher
            .execute(&hook, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap();

        assert_eq!(result.plain_text.as_deref(), Some("PROMPT ran prompt"));
    }

    #[tokio::test]
    async fn dispatch_executor_passes_input_and_context_to_backend() {
        struct AssertingExecutor;

        #[async_trait]
        impl HookExecutor for AssertingExecutor {
            async fn execute(
                &self,
                _hook: &IndividualHookConfig,
                input: &HookInvocationInput,
                ctx: &HookRuntimeContext,
            ) -> Result<ExecutedHookResult, HookExecutionError> {
                assert_eq!(input.session_id, "test-session");
                assert_eq!(ctx.matcher.as_deref(), Some("Bash"));
                assert_eq!(ctx.timeout_override, Some(Duration::from_secs(9)));
                Ok(ExecutedHookResult::plain("ok", 0, "asserting"))
            }
        }

        let dispatcher = DispatchExecutor::new().with_command(Box::new(AssertingExecutor));
        let mut hook = hook_with(HookCommand::Command(BashCommandHook {
            command: "ls".into(),
            r#if: None,
            shell: None,
            timeout: None,
            status_message: None,
            once: None,
            r#async: None,
            async_rewake: None,
        }));
        hook.matcher = Some("Bash".into());
        let input = HookInvocationInput::new(
            HookInvocationContext {
                session_id: "test-session".into(),
                ..HookInvocationContext::default()
            },
            HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            },
        );
        let ctx = HookRuntimeContext {
            matcher: hook.matcher.clone(),
            timeout_override: Some(Duration::from_secs(9)),
        };

        let result = dispatcher.execute(&hook, &input, &ctx).await.unwrap();

        assert_eq!(result.plain_text.as_deref(), Some("ok"));
    }

    #[test]
    fn executed_hook_result_plain_helper_builds_expected_shape() {
        let result = ExecutedHookResult::plain("text", 2, "label");

        assert_eq!(result.plain_text.as_deref(), Some("text"));
        assert!(result.json.is_none());
        assert_eq!(result.validation_error, None);
        assert_eq!(result.exit_code, 2);
        assert_eq!(result.command_label, "label");
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn executed_hook_result_json_helper_builds_expected_shape() {
        let json = HookJsonOutput::Sync(SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::UserPromptSubmit {
                additional_context: Some("ctx".into()),
                session_title: None,
            }),
            ..SyncHookJsonOutput::default()
        });
        let result = ExecutedHookResult::json(json, "hook-cmd");
        assert!(result.json.is_some());
        assert_eq!(result.command_label, "hook-cmd");
        assert_eq!(result.exit_code, 0);
    }
}
