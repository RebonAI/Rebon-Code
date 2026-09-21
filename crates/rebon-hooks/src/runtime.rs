//! The hook runtime: the orchestration layer that sits between the
//! host's event sources and the hook transports.
//!
//! `HookRuntime` owns nothing of its own — it holds references to a
//! [`HookSourceProvider`], a [`EventMetadataMap`], and a
//! [`HookExecutor`]. Every call to [`HookRuntime::run_event`] walks
//! the following pipeline:
//!
//! 1. **Resolve matching hooks.** Fetch the grouping from the
//!    provider, select the matchers that apply to this event, and
//!    filter by the `if`-predicate via the injected
//!    [`IfEvaluator`].
//! 2. **Deduplicate.** Two hooks that are [`is_hook_equal`] (same
//!    command + if + shell) only fire once. Deduplication happens
//!    before dispatch.
//! 3. **Dispatch.** For each surviving hook, call `executor.execute`
//!    in parallel. Collect the per-hook [`ExecutedHookResult`].
//! 4. **Project.** Turn each result into a [`HookResult`] via
//!    `process_hook_json_output` / `classify_plain_command_result`,
//!    then aggregate into an [`AggregatedHookResult`].
//! 5. **Effects.** Project the aggregate into a list of
//!    [`HookEffect`]s the host acts on.
//!
//! ## Where each step lives
//!
//! * `HookRuntime::run_event` — resolve → filter → dispatch
//! * `process_hook_json_output` — project
//! * `aggregate_hook_results` — aggregate
//!
//! ## Why the whole pipeline lives here
//!
//! Splitting the steps across callers is what makes hook behaviour drift:
//! two hosts that each filter, spawn, parse, and merge will eventually
//! disagree about precedence. Keeping all five steps behind one
//! `run_event` gives every caller the same precedence
//! (Deny > Ask > Allow > Passthrough > Defer), the same deduplication,
//! and the same effect projection.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;

use crate::effects::{project_effects, HookEffect};
use crate::event::HookEvent;
use crate::event_metadata::EventMetadataMap;
use crate::executor::{ExecutedHookResult, HookExecutionError, HookExecutor, HookRuntimeContext};
use crate::grouping::{
    group_hooks_by_event_and_matcher, hooks_for_matcher, sorted_matchers_for_event,
    HookSourceProvider,
};
use crate::hook_command::{display_text, is_hook_equal, HookCommand};
use crate::individual_hook::IndividualHookConfig;
use crate::invocation::HookInvocationInput;
use crate::output_protocol::{HookJsonOutput, HookJsonValidationError};
use crate::runtime_result::{
    aggregate_hook_results, classify_plain_command_result, process_hook_json_output,
    AggregatedHookResult, HookResult, ProcessHookJsonError,
};

/// The `if`-predicate evaluator. The implementation is non-
/// trivial (it understands the permission-rule mini-grammar), so the
/// runtime takes it as a trait and the host plugs it in.
///
/// Returning `true` means "fire this hook"; `false` means "skip";
/// `Err(_)` means the predicate errored and the caller should decide
/// whether to skip or fail.
pub trait IfEvaluator: Send + Sync {
    fn evaluate(
        &self,
        predicate: &str,
        hook: &IndividualHookConfig,
        input: &HookInvocationInput,
    ) -> Result<bool, String>;
}

/// Fallback [`IfEvaluator`] that preserves the default: an empty
/// predicate fires, a non-empty predicate fires too (since this
/// impl doesn't understand the grammar — the host is expected to
/// plug in a real evaluator before relying on `if` semantics).
pub struct AllowAllIfEvaluator;

impl IfEvaluator for AllowAllIfEvaluator {
    fn evaluate(
        &self,
        _predicate: &str,
        _hook: &IndividualHookConfig,
        _input: &HookInvocationInput,
    ) -> Result<bool, String> {
        Ok(true)
    }
}

/// What the runtime returns after firing an event: the per-hook
/// decisions, the aggregate, and the projected effects the host
/// should apply.
#[derive(Debug, Clone, PartialEq)]
pub struct HookRuntimeOutput {
    /// Number of hooks the runtime selected after filtering and dedupe.
    pub selected: usize,
    /// Per-hook [`HookResult`], in the order they were dispatched.
    pub per_hook: Vec<HookResult>,
    /// Aggregated result (merged via `aggregate_hook_results`).
    pub aggregated: AggregatedHookResult,
    /// Projection into host-level [`HookEffect`]s.
    pub effects: Vec<HookEffect>,
    /// Executor errors for hooks that failed to run at all. Distinct
    /// from a hook that ran and reported a failing exit code — those
    /// show up in `per_hook` with the appropriate outcome.
    pub execution_errors: Vec<HookExecutionErrorEntry>,
    /// JSON validation errors surfaced during projection (e.g. a
    /// hook whose `hook_specific_output.hookEventName` didn't match
    /// the invocation event).
    pub validation_errors: Vec<HookValidationErrorEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookExecutionErrorEntry {
    pub command: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookValidationErrorEntry {
    pub command: String,
    pub error: String,
}

impl From<ProcessHookJsonError> for HookValidationErrorEntry {
    fn from(err: ProcessHookJsonError) -> Self {
        let (command, error) = match &err {
            ProcessHookJsonError::EventMismatch { .. } => ("<json output>".into(), err.to_string()),
        };
        Self { command, error }
    }
}

impl From<HookJsonValidationError> for HookValidationErrorEntry {
    fn from(err: HookJsonValidationError) -> Self {
        Self {
            command: "<json output>".into(),
            error: err.message,
        }
    }
}

/// Configuration for a runtime firing. The host can pass per-firing
/// overrides (e.g. a tighter timeout for a specific event) without
/// mutating the runtime itself.
#[derive(Debug, Clone, Default)]
pub struct HookRuntimeOptions {
    /// Ceiling applied to every hook this firing.
    pub timeout_override: Option<Duration>,
    /// If true, skip the `is_hook_equal` dedupe pass. Useful in
    /// tests; the host should leave this at its default.
    pub skip_dedupe: bool,
}

/// The orchestrator. Holds references to the three plug-ins it
/// cannot own (provider, metadata, executor) plus the `if`-evaluator
/// and configuration.
///
/// ## Why generic + `Arc`
///
/// Arcs let the runtime be cheap to clone and share across spawned
/// tasks inside `run_event` without requiring the host to worry
/// about 'static bounds. Generics over the provider let the test
/// suite inject a synthetic `HookSourceProvider` without
/// double-boxing.
pub struct HookRuntime<P, E, I = AllowAllIfEvaluator>
where
    P: HookSourceProvider + Send + Sync + 'static,
    E: HookExecutor + 'static,
    I: IfEvaluator + 'static,
{
    provider: Arc<P>,
    metadata: Arc<EventMetadataMap>,
    executor: Arc<E>,
    if_evaluator: Arc<I>,
    options: HookRuntimeOptions,
}

impl<P, E> HookRuntime<P, E, AllowAllIfEvaluator>
where
    P: HookSourceProvider + Send + Sync + 'static,
    E: HookExecutor + 'static,
{
    /// New runtime with the default permissive `if` evaluator.
    pub fn new(provider: Arc<P>, metadata: Arc<EventMetadataMap>, executor: Arc<E>) -> Self {
        Self {
            provider,
            metadata,
            executor,
            if_evaluator: Arc::new(AllowAllIfEvaluator),
            options: HookRuntimeOptions::default(),
        }
    }
}

impl<P, E, I> HookRuntime<P, E, I>
where
    P: HookSourceProvider + Send + Sync + 'static,
    E: HookExecutor + 'static,
    I: IfEvaluator + 'static,
{
    /// New runtime with a custom `if` evaluator.
    pub fn with_if_evaluator(
        provider: Arc<P>,
        metadata: Arc<EventMetadataMap>,
        executor: Arc<E>,
        if_evaluator: Arc<I>,
    ) -> Self {
        Self {
            provider,
            metadata,
            executor,
            if_evaluator,
            options: HookRuntimeOptions::default(),
        }
    }

    pub fn with_options(mut self, options: HookRuntimeOptions) -> Self {
        self.options = options;
        self
    }

    /// Fire an event: resolve, filter, dispatch, aggregate, project.
    pub async fn run_event(&self, input: &HookInvocationInput) -> HookRuntimeOutput {
        let event = input.hook_event_name;
        let hooks = self.select_hooks(event, input);

        if hooks.is_empty() {
            return HookRuntimeOutput {
                selected: 0,
                per_hook: Vec::new(),
                aggregated: AggregatedHookResult::default(),
                effects: Vec::new(),
                execution_errors: Vec::new(),
                validation_errors: Vec::new(),
            };
        }

        let selected = hooks.len();
        let executed = self.execute_all(hooks, input).await;

        let mut per_hook = Vec::with_capacity(executed.len());
        let mut execution_errors = Vec::new();
        let mut validation_errors = Vec::new();

        for entry in executed {
            match entry {
                ExecutionOutcome::Ok { hook, result } => {
                    match project_one_result(&hook, *result, event) {
                        Ok(hook_result) => per_hook.push(hook_result),
                        Err(err) => validation_errors.push(err),
                    }
                }
                ExecutionOutcome::Err { hook, error } => {
                    execution_errors.push(HookExecutionErrorEntry {
                        command: display_text(&hook.config).to_string(),
                        error: error.to_string(),
                    });
                }
            }
        }

        let aggregated = aggregate_hook_results(&per_hook);
        let effects = project_effects(event, &aggregated);

        HookRuntimeOutput {
            selected,
            per_hook,
            aggregated,
            effects,
            execution_errors,
            validation_errors,
        }
    }

    fn select_hooks(
        &self,
        event: HookEvent,
        input: &HookInvocationInput,
    ) -> Vec<IndividualHookConfig> {
        let grouped = group_hooks_by_event_and_matcher(self.provider.as_ref(), &self.metadata);
        let matchers = sorted_matchers_for_event(&grouped, event);
        let matcher_value = input.payload.matcher_value();

        let mut selected: Vec<IndividualHookConfig> = Vec::new();
        for matcher in matchers {
            if !matcher_applies(matcher.as_str(), matcher_value) {
                continue;
            }
            for hook in hooks_for_matcher(&grouped, event, Some(matcher.as_str())) {
                // `if`-predicate filter. Empty / None means "always".
                if let Some(predicate) = hook_if_predicate(&hook.config) {
                    if !predicate.is_empty() {
                        match self.if_evaluator.evaluate(predicate, hook, input) {
                            Ok(true) => {}
                            Ok(false) => continue,
                            Err(_) => continue,
                        }
                    }
                }
                selected.push(hook.clone());
            }
        }

        if self.options.skip_dedupe {
            selected
        } else {
            dedupe_by_identity(selected)
        }
    }

    async fn execute_all(
        &self,
        hooks: Vec<IndividualHookConfig>,
        input: &HookInvocationInput,
    ) -> Vec<ExecutionOutcome> {
        let mut set: JoinSet<(
            usize,
            IndividualHookConfig,
            Result<ExecutedHookResult, HookExecutionError>,
        )> = JoinSet::new();

        for (idx, hook) in hooks.into_iter().enumerate() {
            let executor = self.executor.clone();
            let input = input.clone();
            let ctx = HookRuntimeContext {
                matcher: hook.matcher.clone(),
                timeout_override: self.options.timeout_override,
            };
            set.spawn(async move {
                let result = executor.execute(&hook, &input, &ctx).await;
                (idx, hook, result)
            });
        }

        // Collect in original spawn order so callers get deterministic
        // `per_hook` ordering.
        let mut collected: Vec<Option<ExecutionOutcome>> = (0..set.len()).map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            let (idx, hook, result) = match joined {
                Ok(tuple) => tuple,
                Err(join_err) => {
                    // A panicking hook task is a transport error.
                    // We don't know which hook it was — record it as
                    // a generic entry.
                    collected.push(Some(ExecutionOutcome::Err {
                        hook: placeholder_hook(),
                        error: HookExecutionError::Transport(join_err.to_string()),
                    }));
                    continue;
                }
            };
            collected[idx] = Some(match result {
                Ok(result) => ExecutionOutcome::Ok {
                    hook,
                    result: Box::new(result),
                },
                Err(error) => ExecutionOutcome::Err { hook, error },
            });
        }

        collected.into_iter().flatten().collect()
    }
}

fn placeholder_hook() -> IndividualHookConfig {
    use crate::hook_command::BashCommandHook;
    use crate::hook_source::HookSource;
    IndividualHookConfig {
        event: HookEvent::PreToolUse,
        config: HookCommand::Command(BashCommandHook {
            command: "<panicked>".into(),
            r#if: None,
            shell: None,
            timeout: None,
            status_message: None,
            once: None,
            r#async: None,
            async_rewake: None,
        }),
        matcher: None,
        source: HookSource::UserSettings,
        plugin_name: None,
    }
}

/// A hook's `if` predicate, when set. Matches the `if` property from
/// every `HookCommand` variant.
fn hook_if_predicate(hook: &HookCommand) -> Option<&str> {
    match hook {
        HookCommand::Command(h) => h.r#if.as_deref(),
        HookCommand::Prompt(h) => h.r#if.as_deref(),
        HookCommand::Agent(h) => h.r#if.as_deref(),
        HookCommand::Http(h) => h.r#if.as_deref(),
    }
}

/// Does a hook registered under `matcher` apply to an event firing
/// against the event's matcher value? The runtime does exact-value
/// match, plus `*` / empty wildcard.
fn matcher_applies(matcher: &str, matcher_value: Option<&str>) -> bool {
    match matcher_value {
        None => matcher.is_empty(),
        Some(value) => {
            if matcher.is_empty() || matcher == "*" {
                return true;
            }
            // Regex matching: the runtime treats the matcher as a
            // regex anchored at both ends. Fall back to exact-match
            // when the pattern is not a valid regex.
            match regex::Regex::new(&format!("^(?:{matcher})$")) {
                Ok(re) => re.is_match(value),
                Err(_) => matcher == value,
            }
        }
    }
}

/// `is_hook_equal`-based dedupe. Preserves first-occurrence order so
/// the host-visible `per_hook` ordering preserves runtime behavior.
fn dedupe_by_identity(hooks: Vec<IndividualHookConfig>) -> Vec<IndividualHookConfig> {
    let mut out = Vec::with_capacity(hooks.len());
    for hook in hooks {
        if !out.iter().any(|existing: &IndividualHookConfig| {
            existing.event == hook.event && is_hook_equal(&existing.config, &hook.config)
        }) {
            out.push(hook);
        }
    }
    out
}

#[allow(dead_code)]
fn source_set(hooks: &[IndividualHookConfig]) -> HashSet<crate::hook_source::HookSource> {
    hooks.iter().map(|h| h.source).collect()
}

enum ExecutionOutcome {
    Ok {
        hook: IndividualHookConfig,
        result: Box<ExecutedHookResult>,
    },
    Err {
        hook: IndividualHookConfig,
        error: HookExecutionError,
    },
}

fn project_one_result(
    _hook: &IndividualHookConfig,
    executed: ExecutedHookResult,
    expected_event: HookEvent,
) -> Result<HookResult, HookValidationErrorEntry> {
    let command_label = executed.command_label.clone();

    // JSON path: project via `process_hook_json_output`.
    if let Some(json) = executed.json.as_ref() {
        return match json {
            HookJsonOutput::Async(_) => {
                // Async hooks haven't finished yet — mark as success
                // and let the host poll them separately.
                Ok(HookResult::default())
            }
            HookJsonOutput::Sync(sync) => {
                match process_hook_json_output(sync, &command_label, Some(expected_event)) {
                    Ok(result) => Ok(result),
                    Err(err) => Err(HookValidationErrorEntry {
                        command: command_label,
                        error: err.to_string(),
                    }),
                }
            }
        };
    }

    // Validation-error path: record and classify as non-blocking so
    // the runtime can still aggregate sibling hooks.
    if let Some(err) = executed.validation_error.as_ref() {
        return Err(HookValidationErrorEntry {
            command: command_label,
            error: err.clone(),
        });
    }

    // Plain-text path: exit-code semantics.
    let stderr_for_error = if !executed.stderr.is_empty() {
        executed.stderr.as_str()
    } else {
        executed.plain_text.as_deref().unwrap_or("")
    };
    Ok(classify_plain_command_result(
        executed.exit_code,
        &command_label,
        stderr_for_error,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_metadata::{build_hook_event_metadata, MetadataInputs};
    use crate::executor::DispatchExecutor;
    use crate::grouping::RegisteredHookEntry;
    use crate::hook_command::{BashCommandHook, HookCommand};
    use crate::hook_source::HookSource;
    use crate::invocation::{HookEventPayload, HookInvocationContext};
    use crate::output_protocol::{
        HookJsonOutput, HookPermissionBehavior, HookSpecificOutput, SyncHookJsonOutput,
    };
    use async_trait::async_trait;

    struct MockProvider {
        settings: Vec<IndividualHookConfig>,
    }

    impl HookSourceProvider for MockProvider {
        fn restricted_to_managed_only(&self) -> bool {
            false
        }
        fn hooks_from_settings(&self) -> Vec<IndividualHookConfig> {
            self.settings.clone()
        }
        fn registered_hooks(&self) -> Vec<RegisteredHookEntry> {
            Vec::new()
        }
    }

    /// Executor that replays a pre-seeded JSON decision per hook,
    /// keyed on the command text. Keeps tests deterministic.
    struct ScriptedExecutor {
        decisions: std::collections::HashMap<String, ExecutedHookResult>,
    }

    #[async_trait]
    impl HookExecutor for ScriptedExecutor {
        async fn execute(
            &self,
            hook: &IndividualHookConfig,
            _input: &HookInvocationInput,
            _ctx: &HookRuntimeContext,
        ) -> Result<ExecutedHookResult, HookExecutionError> {
            let label = display_text(&hook.config).to_string();
            self.decisions
                .get(&label)
                .cloned()
                .ok_or_else(|| HookExecutionError::Transport(format!("no decision for {label}")))
        }
    }

    fn metadata() -> EventMetadataMap {
        build_hook_event_metadata(&MetadataInputs {
            tool_names: vec!["Bash".into(), "Read".into()],
            agent_types: vec![],
            elicitation_servers: vec![],
        })
    }

    fn hook(event: HookEvent, matcher: Option<&str>, command: &str) -> IndividualHookConfig {
        IndividualHookConfig {
            event,
            config: HookCommand::Command(BashCommandHook {
                command: command.into(),
                r#if: None,
                shell: None,
                timeout: None,
                status_message: None,
                once: None,
                r#async: None,
                async_rewake: None,
            }),
            matcher: matcher.map(Into::into),
            source: HookSource::UserSettings,
            plugin_name: None,
        }
    }

    fn invocation(payload: HookEventPayload) -> HookInvocationInput {
        HookInvocationInput::new(HookInvocationContext::default(), payload)
    }

    fn deny_json() -> HookJsonOutput {
        HookJsonOutput::Sync(SyncHookJsonOutput {
            hook_specific_output: Some(HookSpecificOutput::PreToolUse {
                permission_decision: Some(HookPermissionBehavior::Deny),
                permission_decision_reason: Some("security policy".into()),
                updated_input: None,
                additional_context: None,
            }),
            ..SyncHookJsonOutput::default()
        })
    }

    #[tokio::test]
    async fn no_matching_hooks_returns_empty_output() {
        let provider = Arc::new(MockProvider { settings: vec![] });
        let executor = Arc::new(ScriptedExecutor {
            decisions: Default::default(),
        });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);
        let out = runtime
            .run_event(&invocation(HookEventPayload::UserPromptSubmit {
                prompt: "hi".into(),
            }))
            .await;
        assert_eq!(out.selected, 0);
        assert!(out.effects.is_empty());
    }

    #[tokio::test]
    async fn pre_tool_use_deny_surfaces_block_effect() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Bash"), "deny.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "deny.sh".into(),
            ExecutedHookResult::json(deny_json(), "deny.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 1);
        assert!(out.effects.iter().any(|e| matches!(
            e,
            HookEffect::BlockToolCall { reason, .. } if reason == "security policy"
        )));
    }

    #[tokio::test]
    async fn matcher_filter_excludes_non_matching_tool() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Read"), "deny.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "deny.sh".into(),
            ExecutedHookResult::json(deny_json(), "deny.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 0);
    }

    #[tokio::test]
    async fn duplicate_hooks_are_deduped() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::PreToolUse, Some("Bash"), "script.sh"),
                hook(HookEvent::PreToolUse, Some("Bash"), "script.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "script.sh".into(),
            ExecutedHookResult::plain("ok", 0, "script.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        // Two registered, one kept.
        assert_eq!(out.selected, 1);
    }

    #[tokio::test]
    async fn execution_error_is_recorded_and_does_not_poison_other_hooks() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::PreToolUse, Some("Bash"), "good.sh"),
                hook(HookEvent::PreToolUse, Some("Bash"), "bad.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "good.sh".into(),
            ExecutedHookResult::plain("ok", 0, "good.sh"),
        );
        // `bad.sh` has no decision → Transport error.
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 2);
        assert_eq!(out.per_hook.len(), 1);
        assert_eq!(out.execution_errors.len(), 1);
        assert_eq!(out.execution_errors[0].command, "bad.sh");
    }

    #[tokio::test]
    async fn wildcard_matcher_fires_on_every_tool() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("*"), "star.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "star.sh".into(),
            ExecutedHookResult::plain("ok", 0, "star.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "SomeNewTool".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 1);
    }

    #[tokio::test]
    async fn regex_matcher_fires_on_multi_tool_pattern() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Bash|Read"), "multi.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "multi.sh".into(),
            ExecutedHookResult::plain("ok", 0, "multi.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Read".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 1);
    }

    #[tokio::test]
    async fn onboarding_phase_matcher_filters_by_phase() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::Onboarding, Some("opened"), "opened.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "opened.sh".into(),
            ExecutedHookResult::plain("ok", 0, "opened.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let opened = invocation(HookEventPayload::Onboarding {
            phase: "opened".into(),
            source: "slash_onboarding".into(),
            step: None,
            previous_step: None,
            next_step: None,
            outcome: None,
            dialog_title: None,
            theme_only: None,
        });
        let out = runtime.run_event(&opened).await;
        assert_eq!(out.selected, 1);

        let closed = invocation(HookEventPayload::Onboarding {
            phase: "closed".into(),
            source: "slash_onboarding".into(),
            step: None,
            previous_step: None,
            next_step: None,
            outcome: None,
            dialog_title: None,
            theme_only: None,
        });
        let out = runtime.run_event(&closed).await;
        assert_eq!(out.selected, 0);
    }

    #[tokio::test]
    async fn non_tool_event_matcher_matches_empty_bucket() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::Stop, None, "cleanup.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "cleanup.sh".into(),
            ExecutedHookResult::plain("bye", 0, "cleanup.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let input = invocation(HookEventPayload::Stop {
            stop_reason: Some("ok".into()),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 1);
    }

    #[tokio::test]
    async fn dispatch_executor_is_usable_as_runtime_backend() {
        // End-to-end sanity: a DispatchExecutor with no registered
        // backends reports UnsupportedType and the runtime records
        // it as an execution error.
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Bash"), "nothing.sh")],
        });
        let dispatcher = DispatchExecutor::new();
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), Arc::new(dispatcher));
        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.execution_errors.len(), 1);
        assert!(out.execution_errors[0].error.contains("unsupported"));
    }

    struct NeverEvaluator;
    impl IfEvaluator for NeverEvaluator {
        fn evaluate(
            &self,
            _predicate: &str,
            _hook: &IndividualHookConfig,
            _input: &HookInvocationInput,
        ) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn if_evaluator_can_filter_everything_out() {
        let provider = Arc::new(MockProvider {
            settings: vec![IndividualHookConfig {
                event: HookEvent::PreToolUse,
                config: HookCommand::Command(BashCommandHook {
                    command: "filtered.sh".into(),
                    r#if: Some("Bash(ls *)".into()),
                    shell: None,
                    timeout: None,
                    status_message: None,
                    once: None,
                    r#async: None,
                    async_rewake: None,
                }),
                matcher: Some("Bash".into()),
                source: HookSource::UserSettings,
                plugin_name: None,
            }],
        });
        let executor = Arc::new(ScriptedExecutor {
            decisions: Default::default(),
        });
        let runtime = HookRuntime::with_if_evaluator(
            provider,
            Arc::new(metadata()),
            executor,
            Arc::new(NeverEvaluator),
        );
        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 0);
    }

    struct RecordingEvaluator {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl IfEvaluator for RecordingEvaluator {
        fn evaluate(
            &self,
            predicate: &str,
            _hook: &IndividualHookConfig,
            _input: &HookInvocationInput,
        ) -> Result<bool, String> {
            self.calls.lock().unwrap().push(predicate.to_string());
            Ok(true)
        }
    }

    struct ErrorEvaluator;

    impl IfEvaluator for ErrorEvaluator {
        fn evaluate(
            &self,
            _predicate: &str,
            _hook: &IndividualHookConfig,
            _input: &HookInvocationInput,
        ) -> Result<bool, String> {
            Err("bad predicate".into())
        }
    }

    #[tokio::test]
    async fn matcher_filter_uses_session_start_source() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::SessionStart, Some("startup"), "startup.sh"),
                hook(HookEvent::SessionStart, Some("resume"), "resume.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "startup.sh".into(),
            ExecutedHookResult::plain("", 0, "startup.sh"),
        );
        decisions.insert(
            "resume.sh".into(),
            ExecutedHookResult::plain("", 0, "resume.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::SessionStart {
                source: "resume".into(),
                model: "opus".into(),
            }))
            .await;

        assert_eq!(out.selected, 1);
        assert_eq!(out.per_hook.len(), 1);
    }

    #[tokio::test]
    async fn matcher_filter_uses_subagent_type() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::SubagentStart, Some("Explore"), "explore.sh"),
                hook(HookEvent::SubagentStart, Some("verification"), "verify.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "explore.sh".into(),
            ExecutedHookResult::plain("", 0, "explore.sh"),
        );
        decisions.insert(
            "verify.sh".into(),
            ExecutedHookResult::plain("", 0, "verify.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::SubagentStart {
                agent_id: "a1".into(),
                agent_type: "verification".into(),
                task: Some("check".into()),
            }))
            .await;

        assert_eq!(out.selected, 1);
        assert_eq!(out.per_hook.len(), 1);
    }

    #[tokio::test]
    async fn matcher_filter_uses_elicitation_server_regex() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(
                HookEvent::Elicitation,
                Some("github|linear"),
                "elicitation.sh",
            )],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "elicitation.sh".into(),
            ExecutedHookResult::plain("", 0, "elicitation.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::Elicitation {
                server: "linear".into(),
                message: "approve?".into(),
                requested_schema: None,
            }))
            .await;

        assert_eq!(out.selected, 1);
    }

    #[tokio::test]
    async fn matcherless_event_only_fires_empty_matcher_bucket() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::UserPromptSubmit, None, "all.sh"),
                hook(HookEvent::SessionStart, Some("should-not-run"), "named.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert("all.sh".into(), ExecutedHookResult::plain("", 0, "all.sh"));
        decisions.insert(
            "named.sh".into(),
            ExecutedHookResult::plain("", 0, "named.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::UserPromptSubmit {
                prompt: "hello".into(),
            }))
            .await;

        assert_eq!(out.selected, 1);
    }

    #[tokio::test]
    async fn invalid_regex_matcher_falls_back_to_exact_match() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::PreToolUse, Some("Bash("), "invalid-regex.sh"),
                hook(HookEvent::PreToolUse, Some("Read("), "other.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "invalid-regex.sh".into(),
            ExecutedHookResult::plain("", 0, "invalid-regex.sh"),
        );
        decisions.insert(
            "other.sh".into(),
            ExecutedHookResult::plain("", 0, "other.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash(".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 1);
    }

    #[tokio::test]
    async fn skip_dedupe_option_allows_duplicate_hooks_to_run() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::PreToolUse, Some("Bash"), "script.sh"),
                hook(HookEvent::PreToolUse, Some("Bash"), "script.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "script.sh".into(),
            ExecutedHookResult::plain("ok", 0, "script.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor).with_options(
            HookRuntimeOptions {
                skip_dedupe: true,
                ..HookRuntimeOptions::default()
            },
        );

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 2);
        assert_eq!(out.per_hook.len(), 2);
    }

    #[tokio::test]
    async fn timeout_override_is_forwarded_to_executor_context() {
        struct ContextExecutor;

        #[async_trait]
        impl HookExecutor for ContextExecutor {
            async fn execute(
                &self,
                hook: &IndividualHookConfig,
                _input: &HookInvocationInput,
                ctx: &HookRuntimeContext,
            ) -> Result<ExecutedHookResult, HookExecutionError> {
                assert_eq!(ctx.matcher.as_deref(), hook.matcher.as_deref());
                assert_eq!(ctx.timeout_override, Some(Duration::from_secs(3)));
                Ok(ExecutedHookResult::plain("", 0, display_text(&hook.config)))
            }
        }

        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Bash"), "ctx.sh")],
        });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), Arc::new(ContextExecutor))
            .with_options(HookRuntimeOptions {
                timeout_override: Some(Duration::from_secs(3)),
                ..HookRuntimeOptions::default()
            });

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 1);
        assert!(out.execution_errors.is_empty());
    }

    #[tokio::test]
    async fn async_json_output_is_success_without_effects() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Bash"), "async.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "async.sh".into(),
            ExecutedHookResult::json(
                HookJsonOutput::Async(crate::output_protocol::AsyncHookJsonOutput {
                    is_async: true,
                    async_timeout: Some(12.5),
                }),
                "async.sh",
            ),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 1);
        assert_eq!(out.per_hook, vec![HookResult::default()]);
        assert!(out.effects.is_empty());
    }

    #[tokio::test]
    async fn validation_error_output_is_recorded_without_blocking_siblings() {
        let provider = Arc::new(MockProvider {
            settings: vec![
                hook(HookEvent::PreToolUse, Some("Bash"), "bad-json.sh"),
                hook(HookEvent::PreToolUse, Some("Bash"), "good.sh"),
            ],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "bad-json.sh".into(),
            ExecutedHookResult {
                json: None,
                plain_text: Some("{bad".into()),
                validation_error: Some("invalid hook JSON".into()),
                exit_code: 0,
                stderr: String::new(),
                command_label: "bad-json.sh".into(),
            },
        );
        decisions.insert(
            "good.sh".into(),
            ExecutedHookResult::plain("", 0, "good.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 2);
        assert_eq!(out.per_hook.len(), 1);
        assert_eq!(out.validation_errors.len(), 1);
        assert_eq!(out.validation_errors[0].command, "bad-json.sh");
        assert!(out.validation_errors[0].error.contains("invalid hook JSON"));
    }

    #[tokio::test]
    async fn mismatched_hook_specific_event_is_validation_error() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Bash"), "mismatch.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "mismatch.sh".into(),
            ExecutedHookResult::json(
                HookJsonOutput::Sync(SyncHookJsonOutput {
                    hook_specific_output: Some(HookSpecificOutput::SessionStart {
                        additional_context: Some("wrong event".into()),
                        initial_user_message: None,
                        watch_paths: None,
                    }),
                    ..SyncHookJsonOutput::default()
                }),
                "mismatch.sh",
            ),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 1);
        assert!(out.per_hook.is_empty());
        assert_eq!(out.validation_errors.len(), 1);
        assert!(out.validation_errors[0]
            .error
            .contains("expected 'PreToolUse' but got 'SessionStart'"));
    }

    #[tokio::test]
    async fn plain_exit_code_two_projects_block_effect() {
        let provider = Arc::new(MockProvider {
            settings: vec![hook(HookEvent::PreToolUse, Some("Bash"), "plain-block.sh")],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "plain-block.sh".into(),
            ExecutedHookResult {
                json: None,
                plain_text: Some("stdout denial".into()),
                validation_error: None,
                exit_code: 2,
                stderr: String::new(),
                command_label: "plain-block.sh".into(),
            },
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        let runtime = HookRuntime::new(provider, Arc::new(metadata()), executor);

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 1);
        assert!(out.effects.iter().any(|effect| matches!(
            effect,
            HookEffect::BlockToolCall { reason, .. } if reason.contains("stdout denial")
        )));
    }

    #[tokio::test]
    async fn if_evaluator_error_skips_hook() {
        let mut conditional = hook(HookEvent::PreToolUse, Some("Bash"), "conditional.sh");
        if let HookCommand::Command(command) = &mut conditional.config {
            command.r#if = Some("Bash(*)".into());
        }
        let provider = Arc::new(MockProvider {
            settings: vec![conditional],
        });
        let executor = Arc::new(ScriptedExecutor {
            decisions: Default::default(),
        });
        let runtime = HookRuntime::with_if_evaluator(
            provider,
            Arc::new(metadata()),
            executor,
            Arc::new(ErrorEvaluator),
        );

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 0);
    }

    #[tokio::test]
    async fn if_evaluator_receives_only_non_empty_predicates() {
        let mut empty_predicate = hook(HookEvent::PreToolUse, Some("Bash"), "empty.sh");
        if let HookCommand::Command(command) = &mut empty_predicate.config {
            command.r#if = Some(String::new());
        }
        let mut real_predicate = hook(HookEvent::PreToolUse, Some("Bash"), "real.sh");
        if let HookCommand::Command(command) = &mut real_predicate.config {
            command.r#if = Some("Bash(ls *)".into());
        }
        let provider = Arc::new(MockProvider {
            settings: vec![empty_predicate, real_predicate],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "empty.sh".into(),
            ExecutedHookResult::plain("", 0, "empty.sh"),
        );
        decisions.insert(
            "real.sh".into(),
            ExecutedHookResult::plain("", 0, "real.sh"),
        );
        let evaluator = Arc::new(RecordingEvaluator {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let runtime = HookRuntime::with_if_evaluator(
            provider,
            Arc::new(metadata()),
            Arc::new(ScriptedExecutor { decisions }),
            evaluator.clone(),
        );

        let out = runtime
            .run_event(&invocation(HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({}),
                tool_use_id: "x".into(),
            }))
            .await;

        assert_eq!(out.selected, 2);
        assert_eq!(evaluator.calls.lock().unwrap().as_slice(), &["Bash(ls *)"]);
    }

    #[tokio::test]
    async fn empty_if_predicate_still_fires() {
        let provider = Arc::new(MockProvider {
            settings: vec![IndividualHookConfig {
                event: HookEvent::PreToolUse,
                config: HookCommand::Command(BashCommandHook {
                    command: "always.sh".into(),
                    r#if: Some(String::new()),
                    shell: None,
                    timeout: None,
                    status_message: None,
                    once: None,
                    r#async: None,
                    async_rewake: None,
                }),
                matcher: Some("Bash".into()),
                source: HookSource::UserSettings,
                plugin_name: None,
            }],
        });
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "always.sh".into(),
            ExecutedHookResult::plain("", 0, "always.sh"),
        );
        let executor = Arc::new(ScriptedExecutor { decisions });
        // Use the NeverEvaluator to prove the predicate is skipped
        // when the `if` string is empty (the runtime short-circuits
        // before calling the evaluator).
        let runtime = HookRuntime::with_if_evaluator(
            provider,
            Arc::new(metadata()),
            executor,
            Arc::new(NeverEvaluator),
        );
        let input = invocation(HookEventPayload::PreToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: "x".into(),
        });
        let out = runtime.run_event(&input).await;
        assert_eq!(out.selected, 1);
    }
}
