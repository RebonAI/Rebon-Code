use super::*;
use crate::turn_hook::{CacheTraceEvent, QueryEventSender, TurnHookContext};

/// Full-replay limit for transient failures before visible output.
pub(super) const MAX_TRANSIENT_REPLAYS: u32 = 8;
#[cfg(test)]
#[derive(Clone)]
struct TransientReplayBackoffProbe {
    entered: std::sync::Arc<tokio::sync::Notify>,
    delay: std::time::Duration,
}
#[cfg(test)]
tokio::task_local! {
    static TRANSIENT_REPLAY_BACKOFF_PROBE: TransientReplayBackoffProbe;
}
/// Exponential retry backoff from 400ms, capped at 10s.
fn transient_replay_backoff(streak: u32) -> std::time::Duration {
    #[cfg(test)]
    if let Ok(probe) = TRANSIENT_REPLAY_BACKOFF_PROBE.try_with(Clone::clone) {
        probe.entered.notify_one();
        return probe.delay;
    }
    let exp = streak.saturating_sub(1).min(5);
    std::time::Duration::from_millis((400u64 << exp).min(10_000))
}

pub(super) fn has_parallel_agent_write_batch(tool_use_list: &[&ToolUseBlock]) -> bool {
    tool_use_list
        .iter()
        .filter(|tool_use| {
            tool_use.name == rebon_tool::AGENT_TOOL_NAME
                && rebon_tool::agent_input_may_write(&tool_use.input)
        })
        .take(2)
        .count()
        > 1
}

fn extend_announced_tools(
    params: &QueryParams,
    announced_tools: &mut std::collections::HashSet<String>,
) {
    announced_tools.extend(params.tools.iter().map(|tool| tool.name.clone()));
    if params
        .tools
        .iter()
        .any(|tool| tool.name == rebon_tool::WORKFLOW_TOOL_NAME)
    {
        announced_tools.insert(rebon_tool::RUN_WORKFLOW_ALIAS.to_string());
    }
}

/// Start the sole owner of live turn control.
pub fn run_query(
    engine: Arc<Engine>,
    session: Arc<SessionHandle>,
    params: QueryParams,
    context: ToolContext,
    cancel: CancelToken,
) -> mpsc::UnboundedReceiver<QueryEvent> {
    let (raw_tx, rx) = mpsc::unbounded_channel();
    let tx = QueryEventSender::new(raw_tx, &params.turn_hooks);
    let context = match params.file_state_cache.as_ref() {
        Some(cache) => context.with_file_state_cache(cache.clone()),
        None if context.file_state_cache().is_none() => {
            context.with_file_state_cache(FileStateCache::new())
        }
        None => context,
    };
    let plugin = TurnControlPlugin {
        engine,
        session,
        params,
        context,
        cancel,
        tx,
    };
    plugin.bind_permission_events();
    tokio::spawn(plugin.run());
    rx
}

/// The only owner of a running query loop.
struct TurnControlPlugin {
    engine: Arc<Engine>,
    session: Arc<SessionHandle>,
    params: QueryParams,
    context: ToolContext,
    cancel: CancelToken,
    tx: QueryEventSender,
}

impl TurnControlPlugin {
    fn bind_permission_events(&self) {
        let tx = self.tx.clone();
        let bind = |broker: &dyn PermissionBroker| {
            if let Some(channel) = channel_permission_broker_from(broker) {
                channel.set_query_event_sender(Some(tx.clone()));
            }
        };
        if let Some(broker) = self.context.permission_broker() {
            bind(broker.as_ref());
        }
        bind(self.engine.permission_broker().as_ref());
    }

    async fn run(mut self) {
        loop {
            match self.drive_once().await {
                None => break,
                Some(messages) => {
                    self.session.reset_session();
                    release_request_policy_after_context_reset(
                        &self.engine,
                        &mut self.params,
                        &mut self.context,
                    );
                    self.params.messages = messages;
                }
            }
        }
    }
}

/// Ends session turn state on every exit path.
pub(super) struct TurnGuard {
    session: Arc<SessionHandle>,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.session.end_turn();
    }
}

const MAX_AUTOMATIC_MAX_TOKEN_CONTINUATIONS: usize = 3;
const MAX_TOKEN_CONTINUATION_PROMPT: &str = "Continue exactly where the previous response stopped. Do not repeat any already emitted text. If you were emitting JSON or code, continue from the next character so the combined output remains valid.";

/// Continue only replayable truncated text/thinking output.
fn can_continue_after_max_tokens(
    message: &AssistantMessage,
    thinking_requires_signature: bool,
) -> bool {
    matches!(message.stop_reason, Some(StopReason::MaxTokens))
        && !message.content.is_empty()
        && message.content.iter().all(|block| match block {
            ApiContentBlock::Text(_) => true,
            ApiContentBlock::Thinking(thinking) => {
                !thinking_requires_signature
                    || thinking
                        .signature
                        .as_deref()
                        .is_some_and(|signature| !signature.is_empty())
                    || thinking
                        .data
                        .as_deref()
                        .is_some_and(|data| !data.is_empty())
            }
            _ => false,
        })
}

fn merge_continued_content(
    mut pending: Vec<ApiContentBlock>,
    current: Vec<ApiContentBlock>,
) -> Vec<ApiContentBlock> {
    pending.extend(current);
    pending
}

/// Rounds in a row a cut-off tool call is handed back to the model
/// before the turn stops. Every such round spends the whole output
/// budget, and a model that has not split the work after two retries
/// is repeating itself.
const MAX_TRUNCATED_TOOL_CALL_ROUNDS: usize = 2;

/// Tool uses that `max_tokens` cut off mid-input.
///
/// The accumulator keeps an input it could not parse as the raw
/// string, so on a `MaxTokens` message a non-object input is the
/// model's own evidence that the call never closed. Once the closing
/// brace was emitted the object parses, and that call is complete even
/// though the message was cut afterwards.
fn truncated_tool_use_ids(message: &AssistantMessage) -> Vec<String> {
    if !matches!(message.stop_reason, Some(StopReason::MaxTokens)) {
        return Vec::new();
    }
    message
        .tool_uses()
        .filter(|tool_use| !tool_use.input.is_object())
        .map(|tool_use| tool_use.id.clone())
        .collect()
}

/// Replace each cut-off input with an empty object so the message can
/// be replayed and persisted: Anthropic rejects a non-object `input`,
/// the Responses API would quote the fragment as a string, and neither
/// the model nor the transcript needs the bytes that were cut.
fn drop_truncated_tool_inputs(message: &mut AssistantMessage, truncated: &[String]) {
    for block in &mut message.content {
        if let ApiContentBlock::ToolUse(tool_use) = block {
            if truncated.contains(&tool_use.id) {
                tool_use.input = Value::Object(Default::default());
            }
        }
    }
}

/// The tool result the model reads for a call that was cut off.
fn truncated_tool_call_result(name: &str, max_tokens: u32) -> String {
    format!(
        "The `{name}` call was cut off by the output token limit (max_tokens = {max_tokens}) \
         before its input was complete, so it was not executed. Emit a smaller call: split the \
         work into several tool calls, and write long content in pieces."
    )
}

/// Materialize promoted transient context for providers without request scope.
fn absorb_promoted_transient_context(
    params: &mut QueryParams,
    manager: &mut ContextManager,
    request_scoped_transient_context: bool,
) {
    if request_scoped_transient_context {
        return;
    }
    let Some(context) = params.transient_context_message.take() else {
        return;
    };
    if context.is_empty() {
        return;
    }
    manager.push_message(virtual_runtime_context_message(&context));
}

fn merge_transient_context(base: Option<String>, dynamic: Option<String>) -> Option<String> {
    match (base, dynamic) {
        (Some(base), Some(dynamic)) if !base.is_empty() && !dynamic.is_empty() => {
            Some(format!("{base}\n\n{dynamic}"))
        }
        (Some(base), _) if !base.is_empty() => Some(base),
        (_, Some(dynamic)) if !dynamic.is_empty() => Some(dynamic),
        _ => None,
    }
}

fn reserve_terminal_continuations(
    iteration: usize,
    max_iterations: usize,
    required_followups: usize,
    remaining: &mut usize,
) {
    let base_iterations_remaining = max_iterations.saturating_sub(iteration.saturating_add(1));
    let required_extensions = required_followups.saturating_sub(base_iterations_remaining);
    *remaining = (*remaining).max(required_extensions);
}

/// Avoid re-injecting an identical skill-listing reminder from replay history.
pub(crate) fn attachment_repeats_history(history: &[ApiMessage], message: &ApiMessage) -> bool {
    if message.role != Role::User || message.content.len() != 1 {
        return false;
    }
    let Some(ApiContentBlock::Text(block)) = message.content.first() else {
        return false;
    };
    if !crate::attachments::is_skill_listing_text(&block.text) {
        return false;
    }
    history.iter().any(|existing| {
        existing.role == Role::User
            && existing.content.len() == 1
            && matches!(
                existing.content.first(),
                Some(ApiContentBlock::Text(existing_block)) if existing_block.text == block.text
            )
    })
}

enum CompactFlow {
    Continue,
    NextIteration,
}

async fn compact_history(
    manager: &mut ContextManager,
    params: &QueryParams,
    session: &Arc<SessionHandle>,
    tx: &QueryEventSender,
    trigger: CompactTrigger,
    instructions: Option<&str>,
) -> (u32, u32, bool) {
    let _ = tx.send(QueryEvent::CompactingStarted {
        messages_before: manager.len(),
    });
    let before_tokens = next_request_input_estimate(manager, params);
    let result =
        compact_with_tail_preservation(manager, params, session, trigger, 4, instructions).await;
    let after_tokens = next_request_input_estimate(manager, params);
    let _ = tx.send(QueryEvent::CompactingFinished {
        messages_after: manager.len(),
        used_model: result.used_model,
    });
    (before_tokens, after_tokens, result.new_baseline_required)
}

/// Report usage, microcompact, or fully compact after a tool round.
#[allow(clippy::too_many_arguments)]
async fn compact_after_tool_round(
    manager: &mut ContextManager,
    params: &QueryParams,
    client: &Arc<dyn ModelClient>,
    session: &Arc<SessionHandle>,
    tx: &QueryEventSender,
    message: &AssistantMessage,
    estimated_input_tokens: u32,
    wants_tools: bool,
    skip_auto_compact: &mut bool,
    pending_cache_miss_reason: &mut CacheMissReason,
) -> CompactFlow {
    if let Some(handle) = &params.prune_level {
        let budget_tokens = next_request_input_estimate(&manager, &params);
        handle.report_estimated_usage(budget_tokens);

        if !*skip_auto_compact {
            // already clear, nothing to do
        } else if !wants_tools {
            (*skip_auto_compact) = false;
        }

        // Rewriting old tool results is net-negative when it invalidates a
        // provider's long prefix-cache hit.
        let preserve_prefix_cache =
            rebon_api::should_preserve_prefix_cache(client.provider_name(), &params.model);

        if wants_tools
            && !*skip_auto_compact
            && !preserve_prefix_cache
            && handle.budget.should_microcompact()
            && !handle.budget.should_auto_compact_for_tokens(budget_tokens)
        {
            let target = handle.budget.microcompact_target();
            let cleared = manager.microcompact_tool_results(
                budget_tokens,
                target,
                /* protected_recent */ 4,
            );
            if cleared > 0 {
                tracing::info!(
                    cleared,
                    input_tokens = message.usage.input_tokens,
                    estimated_input_tokens,
                    budget_tokens,
                    target,
                    "turn_control: microcompact cleared old tool results to head off full compact"
                );
            }
        }

        if wants_tools
            && !*skip_auto_compact
            && handle.budget.should_auto_compact_for_tokens(budget_tokens)
            && mid_turn_compact_allowed(handle, params.max_tokens, budget_tokens)
        {
            let msg_count = manager.len();
            tracing::info!(
                input_tokens = message.usage.input_tokens,
                estimated_input_tokens,
                budget_tokens,
                threshold = handle.budget.auto_compact_threshold(),
                messages = msg_count,
                "turn_control: context budget exceeded, triggering active compact + session reset"
            );

            let (before_tokens, after_tokens, new_baseline_required) =
                compact_history(manager, params, session, tx, CompactTrigger::MidTurn, None).await;
            if new_baseline_required {
                (*pending_cache_miss_reason) = CacheMissReason::CompactReplacedMessages;
            }

            (*skip_auto_compact) = after_tokens >= before_tokens;
            if *skip_auto_compact {
                tracing::warn!(
                    before_tokens,
                    after_tokens,
                    "turn_control: compaction did not reduce estimated tokens, \
                     skipping auto-compact next iteration"
                );
            }

            if *skip_auto_compact {
                handle.budget.record_compact_failure();
            } else {
                handle.budget.record_compact_success();
            }

            manager.append_user_text(
                "[Your previous context was automatically compacted to fit within the context window. Please continue from where you left off. Do not repeat work already completed.]",
            );
            report_estimated_usage_after_compact(&manager, &params);

            return CompactFlow::NextIteration;
        }
    }
    CompactFlow::Continue
}

enum ToolRoundFlow {
    Ran { anchored_promoted: bool },
    Cancelled,
}

/// Dispatch tool calls concurrently and restore provider order.
#[allow(clippy::too_many_arguments)]
async fn run_tool_round(
    engine: &Arc<Engine>,
    manager: &mut ContextManager,
    params: &mut QueryParams,
    context: &mut ToolContext,
    cancel: &CancelToken,
    tx: &QueryEventSender,
    message: &AssistantMessage,
    announced_tools: &mut std::collections::HashSet<String>,
    truncated_tool_uses: &[String],
    auto_mode_classifier_transcript: &str,
) -> ToolRoundFlow {
    let tool_use_list: Vec<_> = message.tool_uses().collect();
    let parallel_agent_write_batch = has_parallel_agent_write_batch(&tool_use_list);
    let tool_batch_context = context
        .clone()
        .with_auto_mode_classifier_transcript(auto_mode_classifier_transcript)
        .with_fresh_file_mutation_batch()
        .with_parallel_agent_write_batch(parallel_agent_write_batch);
    let mut cancelled_during_dispatch = false;

    if cancel.is_cancelled() {
        for tool_use in &tool_use_list {
            let _ = tx.send(QueryEvent::ToolDispatchResult {
                tool_use_id: tool_use.id.clone(),
                name: tool_use.name.clone(),
                outcome: Err("Interrupted by user".into()),
                error_presentation: None,
            });
        }
        cancelled_during_dispatch = true;
    }

    // Keep each result's original index for the model-visible order.
    let mut handles = Vec::with_capacity(tool_use_list.len());
    if !cancelled_during_dispatch {
        for (i, tool_use) in tool_use_list.iter().enumerate() {
            let _ = tx.send(QueryEvent::ToolDispatchStart {
                tool_use_id: tool_use.id.clone(),
                name: tool_use.name.clone(),
                input: tool_use.input.clone(),
            });

            let engine = Arc::clone(&engine);
            let context = tool_batch_context.clone();
            let tx = tx.clone();
            let cancel = cancel.clone();
            let tool_id = tool_use.id.clone();
            let tool_name = tool_use.name.clone();
            let tool_input = tool_use.input.clone();
            let deferred_indexed = context
                .tool_search_index()
                .is_some_and(|index| index.contains_name(&tool_use.name));
            let deferred_available = deferred_indexed
                && (!params.capability_mode.is_minimal()
                    || context.is_deferred_tool_discovered(&tool_use.name));
            let filter_reject = !announced_tools.is_empty()
                && !announced_tools.contains(&tool_use.name)
                && !deferred_available;
            let truncated_input = truncated_tool_uses
                .contains(&tool_use.id)
                .then(|| truncated_tool_call_result(&tool_use.name, params.max_tokens));
            let context = if tool_name == rebon_tool::AGENT_TOOL_NAME {
                tool_input
                    .get("context")
                    .and_then(|value| {
                        serde_json::from_value::<rebon_tool::ContextRequest>(value.clone()).ok()
                    })
                    .and_then(|context_request| {
                        build_parent_context_capsule_from_messages(
                            &context_request,
                            manager.messages(),
                        )
                    })
                    .map(|capsule| context.clone().with_frozen_parent_context(capsule))
                    .unwrap_or(context)
            } else {
                context
            };
            let policy = params.policy.clone();

            handles.push(tokio::spawn(async move {
                let result_tx = tx.clone();
                let result_tool_id = tool_id.clone();
                let result_tool_name = tool_name.clone();
                let dispatch = async move {
                    if let Some(reason) = truncated_input {
                        Err(ToolErrorPresentation::same("output_truncated", reason))
                    } else if filter_reject {
                        Err(ToolErrorPresentation::same(
                            "tool_unavailable",
                            format!("tool `{}` is not available in this session", tool_name),
                        ))
                    } else {
                        let tu = ToolUseBlock {
                            id: tool_id.clone(),
                            name: tool_name.clone(),
                            input: tool_input,
                        };
                        dispatch_tool_use(&engine, &tu, &context, &tx, policy).await
                    }
                };
                tokio::pin!(dispatch);
                let outcome = tokio::select! {
                    biased;
                    _ = cancel.notified() => Err(ToolErrorPresentation::same(
                        "cancelled",
                        "Interrupted by user",
                    )),
                    outcome = &mut dispatch => outcome,
                };
                let error_presentation = outcome
                    .as_ref()
                    .err()
                    .filter(|error| error.has_distinct_display_message())
                    .cloned();
                let event_outcome = outcome.clone().map_err(|error| error.model_message.clone());
                let _ = result_tx.send(QueryEvent::ToolDispatchResult {
                    tool_use_id: result_tool_id.clone(),
                    name: result_tool_name.clone(),
                    outcome: event_outcome,
                    error_presentation,
                });
                (i, result_tool_id, result_tool_name, outcome)
            }));
        }
    }

    let mut indexed_results: Vec<(usize, String, String, Result<Value, ToolErrorPresentation>)> =
        Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.await {
            Ok(result) => indexed_results.push(result),
            Err(join_err) => {
                tracing::error!(error = %join_err, "tool dispatch task panicked");
            }
        }
    }
    indexed_results.sort_by_key(|(i, _, _, _)| *i);

    // Notify synchronously while folding results back into provider order.
    let tools = engine.tool_resolver_for_context(&tool_batch_context);
    let mut tool_results: Vec<ApiContentBlock> = Vec::new();
    for (i, tool_use_id, tool_name, outcome) in &indexed_results {
        let tool_input = tool_use_list.get(*i).map(|tool_use| &tool_use.input);
        tx.dispatch_tool_result(tool_name, outcome.as_ref().ok(), params);
        let permission_extra_text = outcome
            .as_ref()
            .ok()
            .and_then(|value| value.get("permissionExtraText"))
            .and_then(Value::as_str);
        let (mut content, is_error) = match outcome {
            Ok(value) => (
                compact_tool_result_for_model(tool_name, tool_input, value, Some(tools.as_ref())),
                false,
            ),
            Err(err) => (ToolResultContent::text(err.model_message.clone()), true),
        };
        append_permission_extra_text_to_tool_result(&mut content, permission_extra_text);
        tool_results.push(ApiContentBlock::ToolResult(ToolResultBlock {
            tool_use_id: tool_use_id.clone(),
            content,
            is_error,
        }));
    }

    if indexed_results.iter().any(|(_, _, _, outcome)| {
        outcome
            .as_ref()
            .err()
            .map(|error| error.model_message == "Interrupted by user")
            .unwrap_or(false)
    }) && cancel.is_cancelled()
    {
        cancelled_during_dispatch = true;
    }
    if cancelled_during_dispatch {
        let _ = tx.send(QueryEvent::Cancelled);
        return ToolRoundFlow::Cancelled;
    }
    let anchored_promoted = apply_anchored_minimal_promotion(params, context);
    if anchored_promoted {
        extend_announced_tools(&params, announced_tools);
    }

    if params.capability_mode.is_minimal() || anchored_promoted {
        if let Some(index) = context.tool_search_index() {
            for name in context.discovered_deferred_tool_names() {
                let (found, _) = index.select(&[name.as_str()]);
                let Some(entry) = found.first() else {
                    continue;
                };
                let full_tool = ApiTool {
                    name: entry.name.clone(),
                    description: entry.description.clone(),
                    input_schema: entry.input_schema.clone(),
                };
                if let Some(tool) = params.tools.iter_mut().find(|tool| tool.name == name) {
                    *tool = full_tool;
                } else {
                    params.tools.push(full_tool);
                }
                announced_tools.insert(name);
            }
        }
    }

    manager.push_tool_results(tool_results);
    ToolRoundFlow::Ran { anchored_promoted }
}

enum StopReasonFlow {
    Ran,
    NextIteration,
    Stop,
}

/// Settle cancellation, continuation, and terminal subscribers.
#[allow(clippy::too_many_arguments)]
async fn settle_stop_reason(
    manager: &mut ContextManager,
    params: &mut QueryParams,
    context: &mut ToolContext,
    client: &Arc<dyn ModelClient>,
    cancel: &CancelToken,
    tx: &QueryEventSender,
    message: &AssistantMessage,
    iteration: usize,
    wants_tools: bool,
    total_usage: &mut Usage,
    stop_reason_final: &mut Option<StopReason>,
    terminal_continuations_remaining: &mut usize,
) -> StopReasonFlow {
    // Preserve partial tool-use transcript validity on cancellation.
    if cancel.is_cancelled() {
        for tool_use in message.tool_uses() {
            let _ = tx.send(QueryEvent::ToolDispatchResult {
                tool_use_id: tool_use.id.clone(),
                name: tool_use.name.clone(),
                outcome: Err("Interrupted by user".into()),
                error_presentation: None,
            });
        }
        let _ = tx.send(QueryEvent::Cancelled);
        return StopReasonFlow::Stop;
    }

    if !wants_tools {
        // The ordered terminal waterfall stops at its first continuation.
        tx.dispatch_turn_end(
            message,
            iteration,
            manager.messages(),
            &context.task_list_id(),
            params,
        );
        let applied = tx.take_writeback().apply_to_params(params);
        if let Some(followups) = applied.continue_followups {
            tracing::info!("turn hook requested another iteration");
            if params.next_tool_choice.is_some() && !client.supports_forced_tool_choice() {
                params.next_tool_choice = None;
                tracing::debug!(
                    provider = client.provider_name(),
                    "turn hook requested forced tool_choice but provider does not advertise support"
                );
            }
            reserve_terminal_continuations(
                iteration,
                params.max_iterations,
                followups,
                &mut (*terminal_continuations_remaining),
            );
            // Usage precedes the terminal phase's writebacks.
            manager.set_usage_baseline(message.usage.input_tokens);
            commit_turn_hook_writeback(manager, context, tx, iteration + 1, applied);
            return StopReasonFlow::NextIteration;
        }
        commit_turn_hook_writeback(manager, context, tx, iteration + 1, applied);
        let _ = tx.send(QueryEvent::Done {
            final_message: message.clone(),
            stop_reason: stop_reason_final.clone().unwrap_or(StopReason::EndTurn),
            total_usage: *total_usage,
        });
        return StopReasonFlow::Stop;
    }
    StopReasonFlow::Ran
}

enum StreamOutcomeFlow {
    Ran {
        message: AssistantMessage,
        wants_tools: bool,
        /// Tool uses `max_tokens` cut short; the tool round answers
        /// them with an error instead of running them.
        truncated_tool_uses: Vec<String>,
    },
    NextIteration,
    Stop,
}

/// Finalize a successful stream and decide continuation.
#[allow(clippy::too_many_arguments)]
async fn settle_stream_outcome(
    manager: &mut ContextManager,
    params: &mut QueryParams,
    context: &mut ToolContext,
    client: &Arc<dyn ModelClient>,
    tx: &QueryEventSender,
    accumulator: MessageAccumulator,
    iteration: usize,
    announced_tools: &mut std::collections::HashSet<String>,
    total_usage: &mut Usage,
    stop_reason_final: &mut Option<StopReason>,
    pending_max_token_content: &mut Option<Vec<ApiContentBlock>>,
    automatic_max_token_continuations: &mut usize,
    truncated_tool_call_rounds: &mut usize,
    transient_replay_streak: &mut u32,
    request_scoped_transient_context: bool,
    base_transient_context_message: &mut Option<String>,
    request_cache_trace_context: &Option<rebon_api::CacheTraceContext>,
    last_message: &mut Option<AssistantMessage>,
) -> StreamOutcomeFlow {
    (*transient_replay_streak) = 0;

    let mut message = accumulator.finish();
    (*total_usage).merge(&message.usage);
    if let Some(handle) = &params.prune_level {
        handle.report_usage(message.usage.input_tokens);
    }
    tx.emit_cache_trace(&CacheTraceEvent::RequestUsage {
        model: &params.model,
        usage: &message.usage,
        cache_trace_context: request_cache_trace_context.as_ref(),
    });
    let truncated_tool_uses = truncated_tool_use_ids(&message);
    if truncated_tool_uses.is_empty() {
        (*truncated_tool_call_rounds) = 0;
    } else {
        // The provider-side continuation would point at a response whose
        // function_call never closed; the next request replays the local
        // history, where the call is paired with the error it gets below.
        client.invalidate_previous_response_id();
        drop_truncated_tool_inputs(&mut message, &truncated_tool_uses);
        if *truncated_tool_call_rounds >= MAX_TRUNCATED_TOOL_CALL_ROUNDS {
            tracing::warn!(
                model = %params.model,
                rounds = *truncated_tool_call_rounds + 1,
                "turn_control: model kept emitting tool calls that hit max_tokens; stopping the turn"
            );
            // Commit the message ahead of its results so the transcript
            // pairs every tool_use with the error that answers it.
            if tx
                .send(QueryEvent::IterationComplete {
                    iteration,
                    message: message.clone(),
                })
                .is_err()
            {
                return StreamOutcomeFlow::Stop;
            }
            let reason = format!(
                "Model output was truncated while emitting a tool call {} times in a row; the tool calls in this response were not executed. Increase the output token budget or ask for the work in smaller steps.",
                MAX_TRUNCATED_TOOL_CALL_ROUNDS + 1
            );
            for tool_use in message.tool_uses() {
                let _ = tx.send(QueryEvent::ToolDispatchResult {
                    tool_use_id: tool_use.id.clone(),
                    name: tool_use.name.clone(),
                    outcome: Err(reason.clone()),
                    error_presentation: None,
                });
            }
            let _ = tx.send(QueryEvent::Error(reason));
            return StreamOutcomeFlow::Stop;
        }
        (*truncated_tool_call_rounds) += 1;
        tracing::warn!(
            model = %params.model,
            round = *truncated_tool_call_rounds,
            max_rounds = MAX_TRUNCATED_TOOL_CALL_ROUNDS,
            tool_uses = ?truncated_tool_uses,
            "turn_control: max_tokens cut a tool call short; handing it back to the model as a tool error"
        );
    }

    if can_continue_after_max_tokens(&message, client.thinking_replay_requires_signature())
        && *automatic_max_token_continuations < MAX_AUTOMATIC_MAX_TOKEN_CONTINUATIONS
    {
        // Do not schedule a continuation beyond the final base iteration.
        if iteration + 1 < params.max_iterations {
            tracing::warn!(
                model = %params.model,
                continuation = (*automatic_max_token_continuations) + 1,
                max_continuations = MAX_AUTOMATIC_MAX_TOKEN_CONTINUATIONS,
                "turn_control: model hit max_tokens; requesting an automatic continuation"
            );
            (*pending_max_token_content) = Some(match (*pending_max_token_content).take() {
                Some(pending) => merge_continued_content(pending, message.content.clone()),
                None => message.content.clone(),
            });
            manager.set_usage_baseline(message.usage.input_tokens);
            manager.push_message(ApiMessage {
                role: Role::Assistant,
                content: message.content.clone(),
            });
            manager.push_message(ApiMessage::user_text(MAX_TOKEN_CONTINUATION_PROMPT));
            (*automatic_max_token_continuations) += 1;
            return StreamOutcomeFlow::NextIteration;
        }
        tracing::warn!(
            model = %params.model,
            "turn_control: model hit max_tokens on the final iteration; emitting the truncated response instead of continuing"
        );
    }

    if let Some(pending) = (*pending_max_token_content).take() {
        message.content = merge_continued_content(pending, message.content);
    }
    (*automatic_max_token_continuations) = 0;

    if tx
        .send(QueryEvent::IterationComplete {
            iteration,
            message: message.clone(),
        })
        .is_err()
    {
        return StreamOutcomeFlow::Stop;
    }

    (*stop_reason_final) = message.stop_reason.clone();
    (*last_message) = Some(message.clone());

    let wants_tools =
        matches!(message.stop_reason, Some(StopReason::ToolUse)) || message.has_tool_use();
    if !wants_tools
        && anchored_minimal_content_has_anchor(&Role::Assistant, &message.content)
        && apply_anchored_minimal_promotion(params, context)
    {
        absorb_promoted_transient_context(params, manager, request_scoped_transient_context);
        (*base_transient_context_message) = params.transient_context_message.clone();
        extend_announced_tools(&params, announced_tools);
    }
    StreamOutcomeFlow::Ran {
        message,
        wants_tools,
        truncated_tool_uses,
    }
}

enum PreTurnCompactFlow {
    Ran {
        estimated_input_tokens: u32,
        cache_miss_reason: CacheMissReason,
    },
    NextIteration,
}

/// Handle manual, token-target, and pre-turn auto-compaction.
#[allow(clippy::too_many_arguments)]
async fn compact_before_request(
    manager: &mut ContextManager,
    params: &mut QueryParams,
    session: &Arc<SessionHandle>,
    tx: &QueryEventSender,
    iteration: usize,
    skip_auto_compact: &mut bool,
    pending_cache_miss_reason: &mut CacheMissReason,
) -> PreTurnCompactFlow {
    let (manual_compact_requested, manual_compact_instructions) = params
        .prune_level
        .as_ref()
        .map(|handle| handle.budget.take_compact_once_with_instructions())
        .unwrap_or((false, None));
    if manual_compact_requested {
        let msg_count = manager.len();
        tracing::info!(
            messages = msg_count,
            "turn_control: manual /compact requested, triggering active compact + session reset"
        );

        let (before_tokens, after_tokens, new_baseline_required) = compact_history(
            manager,
            params,
            session,
            tx,
            CompactTrigger::Manual,
            manual_compact_instructions.as_deref(),
        )
        .await;
        if new_baseline_required {
            (*pending_cache_miss_reason) = CacheMissReason::CompactReplacedMessages;
        }

        (*skip_auto_compact) = after_tokens >= before_tokens;
        if *skip_auto_compact {
            tracing::warn!(
                before_tokens,
                after_tokens,
                "turn_control: manual compaction did not reduce estimated tokens, skipping auto-compact next iteration"
            );
        }

        report_estimated_usage_after_compact(&manager, &params);

        return PreTurnCompactFlow::NextIteration;
    }

    let estimated_input_tokens = next_request_input_estimate(&manager, &params);
    let mut cache_miss_reason =
        std::mem::replace(&mut (*pending_cache_miss_reason), CacheMissReason::None);
    if let Some(target_tokens) =
        hard_context_guard_target(params.prune_level.as_ref(), params.max_tokens)
    {
        if estimated_input_tokens > target_tokens {
            let history_target_tokens =
                history_target_for_request_budget(&manager, &params, target_tokens);
            let report = manager.truncate_for_token_budget(
                history_target_tokens,
                REPLAY_MAX_TAIL_MESSAGES,
                min_tail_messages_preserving_runtime_context(
                    manager.messages(),
                    REPLAY_MIN_TAIL_MESSAGES,
                ),
            );
            if report.changed() {
                tracing::info!(
                    before_tokens = report.before_tokens,
                    after_tokens = report.after_tokens,
                    before_messages = report.before_messages,
                    after_messages = report.after_messages,
                    target_tokens = history_target_tokens,
                    request_target_tokens = target_tokens,
                    "turn_control: hard context guard pruned history before request"
                );
                session.invalidate_continuation();
                cache_miss_reason = CacheMissReason::HardContextGuardTruncated;
                report_estimated_usage_after_compact(&manager, &params);
            }
        }
    }
    let estimated_input_tokens = next_request_input_estimate(&manager, &params);
    let pre_turn_budget_tokens = params
        .prune_level
        .as_ref()
        .map(|handle| std::cmp::max(estimated_input_tokens, handle.budget.last_input_tokens()));
    let pre_turn_auto_compact = iteration == 0
        && !*skip_auto_compact
        && params.prune_level.as_ref().is_some_and(|handle| {
            pre_turn_budget_tokens
                .is_some_and(|tokens| handle.budget.should_auto_compact_for_tokens(tokens))
        });
    if pre_turn_auto_compact {
        let msg_count = manager.len();
        if manager.is_empty() {
            // Nothing to compact.
        } else {
            tracing::info!(
                messages = msg_count,
                threshold = params
                    .prune_level
                    .as_ref()
                    .map(|handle| handle.budget.auto_compact_threshold())
                    .unwrap_or_default(),
                estimated_input_tokens,
                budget_tokens = pre_turn_budget_tokens.unwrap_or_default(),
                "turn_control: pre-turn context budget exceeded, compacting prior history"
            );

            let (before_tokens, after_tokens, new_baseline_required) = compact_history(
                manager,
                params,
                session,
                tx,
                CompactTrigger::AutoPreTurn,
                None,
            )
            .await;
            if new_baseline_required {
                cache_miss_reason = CacheMissReason::CompactReplacedMessages;
            }

            (*skip_auto_compact) = after_tokens >= before_tokens;
            if let Some(handle) = &params.prune_level {
                if *skip_auto_compact {
                    handle.budget.record_compact_failure();
                } else {
                    handle.budget.record_compact_success();
                }
            }
            if *skip_auto_compact {
                tracing::warn!(
                    before_tokens,
                    after_tokens,
                    "turn_control: pre-turn compaction did not reduce estimated tokens, skipping auto-compact next iteration"
                );
            }
        }
    }
    PreTurnCompactFlow::Ran {
        estimated_input_tokens,
        cache_miss_reason,
    }
}

enum StreamDrainFlow {
    Ran { accumulator: MessageAccumulator },
    NextIteration,
    Stop,
}

/// Open and drain one model stream with retry/cancel handling.
#[allow(clippy::too_many_arguments)]
async fn open_and_drain_stream(
    manager: &mut ContextManager,
    params: &mut QueryParams,
    session: &Arc<SessionHandle>,
    client: &Arc<dyn ModelClient>,
    cancel: &CancelToken,
    tx: &QueryEventSender,
    request: rebon_api::CreateMessageRequest,
    context_overflow_retried: &mut bool,
    last_request_failure: &mut Option<String>,
    transient_replay_streak: &mut u32,
    pending_cache_miss_reason: &mut CacheMissReason,
) -> StreamDrainFlow {
    let mut stream = match tokio::select! {
        biased;
        _ = cancel.notified() => {
            let _ = tx.send(QueryEvent::Cancelled);
            return StreamDrainFlow::Stop;
        }
        result = client.create_message_stream(request) => result,
    } {
        Ok(s) => {
            (*last_request_failure) = None;
            s
        }
        Err(ref err) if err.context_overflow().is_some() && !*context_overflow_retried => {
            tracing::warn!(
                error = %err,
                "turn_control: context overflow at request level, \
                 invalidating previous_response_id, shrinking context, and retrying"
            );
            session.invalidate_continuation();
            (*pending_cache_miss_reason) = CacheMissReason::OverflowInvalidatedPreviousResponseId;
            shrink_context_for_overflow_retry(manager, &params, &session);
            (*context_overflow_retried) = true;
            return StreamDrainFlow::NextIteration;
        }
        Err(err) if err.is_transient() => {
            (*last_request_failure) = Some(format!("model stream start failed: {err}"));
            (*transient_replay_streak) += 1;
            if (*transient_replay_streak) > MAX_TRANSIENT_REPLAYS {
                let _ = tx.send(QueryEvent::Error(format!(
                    "model stream start failed {MAX_TRANSIENT_REPLAYS} times in a row; giving up: {err}"
                )));
                return StreamDrainFlow::Stop;
            }
            tracing::warn!(
                error = %err,
                attempt = (*transient_replay_streak),
                max_attempts = MAX_TRANSIENT_REPLAYS,
                "turn_control: stream start failed before visible output, retrying with full replay"
            );
            session.invalidate_continuation();
            (*pending_cache_miss_reason) = CacheMissReason::RetryWithoutPreviousResponseId;
            tokio::select! {
                biased;
                _ = cancel.notified() => {
                    let _ = tx.send(QueryEvent::Cancelled);
                    return StreamDrainFlow::Stop;
                }
                _ = tokio::time::sleep(transient_replay_backoff(*transient_replay_streak)) => {}
            }
            return StreamDrainFlow::NextIteration;
        }
        Err(err) => {
            if err.context_overflow().is_some() {
                tracing::warn!(
                    error = %err,
                    "turn_control: context overflow persisted after retry; applying emergency shrink for next turn"
                );
                session.invalidate_continuation();
                shrink_context_for_overflow_retry(manager, &params, &session);
                (*pending_cache_miss_reason) =
                    CacheMissReason::OverflowInvalidatedPreviousResponseId;
                (*context_overflow_retried) = false;
                return StreamDrainFlow::NextIteration;
            }
            let _ = tx.send(QueryEvent::Error(format!(
                "model stream start failed: {err}"
            )));
            return StreamDrainFlow::Stop;
        }
    };

    let mut accumulator = MessageAccumulator::new();
    let mut visible_output_started = false;
    loop {
        // Finish the partial message instead of discarding it on cancel.
        if cancel.is_cancelled() {
            break;
        }
        let next = tokio::select! {
            biased;
            _ = cancel.notified() => None,
            next = stream.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(ref err))
                if err.context_overflow().is_some() && !*context_overflow_retried =>
            {
                tracing::warn!(
                    error = %err,
                    "turn_control: context overflow in stream, \
                     invalidating previous_response_id, shrinking context, and retrying"
                );
                session.invalidate_continuation();
                (*pending_cache_miss_reason) =
                    CacheMissReason::OverflowInvalidatedPreviousResponseId;
                shrink_context_for_overflow_retry(manager, &params, &session);
                (*context_overflow_retried) = true;
                return StreamDrainFlow::NextIteration;
            }
            Some(Err(err)) if err.is_transient() && !visible_output_started => {
                (*last_request_failure) = Some(format!("model stream error: {err}"));
                (*transient_replay_streak) += 1;
                if (*transient_replay_streak) > MAX_TRANSIENT_REPLAYS {
                    let _ = tx.send(QueryEvent::Error(format!(
                        "model stream failed {MAX_TRANSIENT_REPLAYS} times in a row without producing output; giving up: {err}"
                    )));
                    return StreamDrainFlow::Stop;
                }
                tracing::warn!(
                    error = %err,
                    attempt = (*transient_replay_streak),
                    max_attempts = MAX_TRANSIENT_REPLAYS,
                    "turn_control: stream failed before visible output, retrying with full replay"
                );
                session.invalidate_continuation();
                (*pending_cache_miss_reason) = CacheMissReason::RetryWithoutPreviousResponseId;
                tokio::select! {
                    biased;
                    _ = cancel.notified() => {
                        let _ = tx.send(QueryEvent::Cancelled);
                        return StreamDrainFlow::Stop;
                    }
                    _ = tokio::time::sleep(transient_replay_backoff(*transient_replay_streak)) => {}
                }
                return StreamDrainFlow::NextIteration;
            }
            Some(Err(err)) => {
                if err.context_overflow().is_some() {
                    tracing::warn!(
                        error = %err,
                        "turn_control: context overflow persisted after stream retry; applying emergency shrink for next turn"
                    );
                    session.invalidate_continuation();
                    shrink_context_for_overflow_retry(manager, &params, &session);
                    (*pending_cache_miss_reason) =
                        CacheMissReason::OverflowInvalidatedPreviousResponseId;
                    (*context_overflow_retried) = false;
                    return StreamDrainFlow::NextIteration;
                }
                let _ = tx.send(QueryEvent::Error(format!("model stream error: {err}")));
                return StreamDrainFlow::Stop;
            }
            None => break,
        };
        if tx.send(QueryEvent::Stream(event.clone())).is_err() {
            return StreamDrainFlow::Stop;
        }
        match &event {
            StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                rebon_api::ContentBlockDelta::TextDelta { text } if !text.is_empty() => {
                    visible_output_started = true;
                }
                rebon_api::ContentBlockDelta::ThinkingDelta { thinking }
                    if !thinking.is_empty() =>
                {
                    visible_output_started = true;
                }
                rebon_api::ContentBlockDelta::InputJsonDelta { partial_json }
                    if !partial_json.is_empty() =>
                {
                    visible_output_started = true;
                }
                rebon_api::ContentBlockDelta::ImageDataDelta { b64_json, .. }
                    if !b64_json.is_empty() =>
                {
                    visible_output_started = true;
                }
                _ => {}
            },
            StreamEvent::ContentBlockStart { content_block, .. } => match content_block {
                rebon_api::ContentBlockStart::ToolUse { .. }
                | rebon_api::ContentBlockStart::ServerToolUse { .. }
                | rebon_api::ContentBlockStart::ImageGeneration { .. } => {
                    visible_output_started = true;
                }
                _ => {}
            },
            _ => {}
        }
        if let Err(err) = accumulator.apply(&event) {
            let _ = tx.send(QueryEvent::Error(format!(
                "accumulator rejected event: {err}"
            )));
            return StreamDrainFlow::Stop;
        }
    }
    StreamDrainFlow::Ran { accumulator }
}

enum PostRoundFlow {
    Ran,
    NextIteration,
    Reset(Vec<ApiMessage>),
}

/// Apply ordered post-tool-round control phases.
#[allow(clippy::too_many_arguments)]
async fn finish_tool_round(
    manager: &mut ContextManager,
    params: &mut QueryParams,
    context: &mut ToolContext,
    session: &Arc<SessionHandle>,
    client: &Arc<dyn ModelClient>,
    tx: &QueryEventSender,
    message: &AssistantMessage,
    iteration: usize,
    wants_tools: bool,
    anchored_promoted: bool,
    estimated_input_tokens: u32,
    request_scoped_transient_context: bool,
    base_transient_context_message: &mut Option<String>,
    skip_auto_compact: &mut bool,
    pending_cache_miss_reason: &mut CacheMissReason,
) -> PostRoundFlow {
    // Promotion must follow tool results to preserve tool_use/result adjacency.
    if anchored_promoted {
        absorb_promoted_transient_context(params, manager, request_scoped_transient_context);
        (*base_transient_context_message) = params.transient_context_message.clone();
    }

    let hook_writeback = tx.dispatch_tool_round(message, iteration, params).await;
    let _ = apply_turn_hook_writeback(manager, params, context, tx, iteration + 1, hook_writeback);

    if let Some(poller) = params.attachment_poller.as_ref() {
        // Reset requires a fresh provider session, not an in-loop history swap.
        if let Some(reset_messages) = poller.poller.take_context_reset() {
            // Expose plan text in the cleared transcript.
            let plan = reset_messages.first().and_then(|msg| {
                msg.content.iter().find_map(|block| match block {
                    rebon_api::ContentBlock::Text(text) => text
                        .text
                        .strip_prefix("Implement the following plan:\n\n")
                        .map(str::to_owned)
                        .or_else(|| Some(text.text.clone())),
                    _ => None,
                })
            });
            let _ = tx.send(QueryEvent::ContextReset {
                messages: reset_messages.clone(),
                plan,
            });
            return PostRoundFlow::Reset(reset_messages);
        } else if iteration + 1 < params.max_iterations {
            let writeback = tx.dispatch_attachment_poll(
                AttachmentPollPhase::Regular,
                (iteration as u64).saturating_add(1),
                manager.messages(),
                params.attachment_poller.as_ref(),
            );
            let _ =
                apply_turn_hook_writeback(manager, params, context, tx, iteration + 1, writeback);
        }
    }

    if matches!(
        compact_after_tool_round(
            manager,
            &params,
            &client,
            &session,
            &tx,
            &message,
            estimated_input_tokens,
            wants_tools,
            skip_auto_compact,
            pending_cache_miss_reason,
        )
        .await,
        CompactFlow::NextIteration
    ) {
        return PostRoundFlow::NextIteration;
    }
    PostRoundFlow::Ran
}

/// The request this iteration sends, and the cache trace snapshot that goes with it.
struct BuiltRequest {
    request: rebon_api::CreateMessageRequest,
    request_cache_trace_context: Option<rebon_api::CacheTraceContext>,
}

/// 在首次 attachment poll 及压缩之后准备本次 provider 请求。
fn build_iteration_request(
    control: &TurnControlPlugin,
    fresh_history: bool,
    manager: &mut ContextManager,
    params: &mut QueryParams,
    context: &mut ToolContext,
    tx: &QueryEventSender,
    iteration: usize,
    cache_miss_reason: CacheMissReason,
) -> BuiltRequest {
    if iteration == 0 {
        let writeback = tx.dispatch_attachment_poll(
            AttachmentPollPhase::Eager,
            0,
            manager.messages(),
            params.attachment_poller.as_ref(),
        );
        let _ = apply_turn_hook_writeback(manager, params, context, tx, 0, writeback);
    }

    if let Some(notification) = code_mode::prepare(
        &control.engine,
        &control.session,
        params,
        context,
        manager.messages(),
        fresh_history,
    ) {
        let writeback = notification.dispatch(tx, manager.messages(), params, iteration);
        let _ = apply_turn_hook_writeback(manager, params, context, tx, iteration, writeback);
    }

    let request = build_model_request(&manager, &params);
    params.next_tool_choice = None;
    let request_cache_trace_context = request.cache_trace_context.clone();
    tx.emit_cache_trace(&CacheTraceEvent::RequestBuilt {
        request: &request,
        runtime_context_message: params.runtime_context_message.as_deref(),
        transient_context_message: params.transient_context_message.as_deref(),
        cache_miss_reason,
    });
    BuiltRequest {
        request,
        request_cache_trace_context,
    }
}

/// Initial provider-visible tool names used by dispatch enforcement.
fn initial_announced_tools(params: &QueryParams) -> std::collections::HashSet<String> {
    params
        .tools
        .iter()
        .map(|t| t.name.clone())
        .chain(
            std::iter::once(rebon_tool::RUN_WORKFLOW_ALIAS.to_string()).filter(|_| {
                params
                    .tools
                    .iter()
                    .any(|tool| tool.name == rebon_tool::WORKFLOW_TOOL_NAME)
            }),
        )
        .chain(std::iter::once(
            rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME.to_string(),
        ))
        .collect()
}

/// Materialize transient context unless the provider supports request scope.
fn prime_transient_context(
    params: &mut QueryParams,
    client: &Arc<dyn ModelClient>,
) -> (bool, Option<String>) {
    let request_scoped_transient_context = client.supports_request_scoped_transient_context();
    let base_transient_context_message = params.transient_context_message.clone();
    if !request_scoped_transient_context {
        let dynamic = params.attachment_poller.as_ref().and_then(|binding| {
            binding
                .poller
                .transient_context_for_query(&binding.session_id, &binding.turn_id)
        });
        params.transient_context_message =
            merge_transient_context(base_transient_context_message.clone(), dynamic);
        materialize_durable_transient_context(
            &mut params.messages,
            &mut params.transient_context_message,
        );
    }
    (
        request_scoped_transient_context,
        base_transient_context_message,
    )
}

/// Commit hook history, attachment events, and context paths in order.
fn commit_turn_hook_writeback(
    manager: &mut ContextManager,
    context: &mut ToolContext,
    tx: &QueryEventSender,
    announce_iteration: usize,
    applied: TurnHookContext,
) {
    context.extend_coordinator_report_paths(applied.coordinator_report_paths);
    for message in applied.history {
        manager.push_message(message);
    }
    for attachment in applied.injected_attachments {
        let _ = tx.send(QueryEvent::AttachmentInjected {
            iteration: announce_iteration,
            message: attachment.clone(),
        });
        let model_message = model_message_for_attachment(&attachment);
        if !model_message.content.is_empty() {
            manager.push_message(model_message);
        }
    }
}

/// Apply parameter updates and commit one phase's writeback.
fn apply_turn_hook_writeback(
    manager: &mut ContextManager,
    params: &mut QueryParams,
    context: &mut ToolContext,
    tx: &QueryEventSender,
    announce_iteration: usize,
    writeback: TurnHookContext,
) -> Option<usize> {
    let applied = writeback.apply_to_params(params);
    let followups = applied.continue_followups;
    commit_turn_hook_writeback(manager, context, tx, announce_iteration, applied);
    followups
}

fn apply_turn_hook_writebacks(
    manager: &mut ContextManager,
    params: &mut QueryParams,
    context: &mut ToolContext,
    tx: &QueryEventSender,
    announce_iteration: usize,
) -> Option<usize> {
    apply_turn_hook_writeback(
        manager,
        params,
        context,
        tx,
        announce_iteration,
        tx.take_writeback(),
    )
}

impl TurnControlPlugin {
    async fn drive_once(&self) -> Option<Vec<ApiMessage>> {
        let engine = self.engine.clone();
        let session = self.session.clone();
        let mut params = self.params.clone();
        let fresh_history = code_mode::fresh_history(&params.messages);
        let mut context = self.context.clone();
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        let _turn_guard = TurnGuard {
            session: session.clone(),
        };
        // Context reset restarts every subscriber's drive-local state.
        tx.begin_drive();
        let client = session.client_arc();

        if let Some(handle) = query_prune_handle(&params, &session) {
            handle.set_context_window_for_model(&params.model);
            if params.prune_level.is_none() {
                params.prune_level = Some(handle);
            }
        }

        let mut announced_tools = initial_announced_tools(&params);
        let mut total_usage = Usage::default();
        let mut last_message: Option<AssistantMessage> = None;
        let mut stop_reason_final: Option<StopReason> = None;
        let mut last_request_failure: Option<String> = None;
        // Overflow retry is shared by request-level and in-stream errors.
        let mut context_overflow_retried = false;
        // Count and back off transient replays until progress resets the streak.
        let mut transient_replay_streak: u32 = 0;
        // Break a compact loop when history cannot shrink further.
        let mut skip_auto_compact = false;
        // ContextManager is the sole live owner of query history.
        let (request_scoped_transient_context, mut base_transient_context_message) =
            prime_transient_context(&mut params, &client);
        let mut manager =
            ContextManager::new(params.system.clone(), std::mem::take(&mut params.messages));
        let mut pending_cache_miss_reason = CacheMissReason::None;
        let mut pending_max_token_content: Option<Vec<ApiContentBlock>> = None;
        let mut automatic_max_token_continuations = 0usize;
        let mut truncated_tool_call_rounds = 0usize;
        let mut terminal_continuations_remaining = 0usize;
        let mut iterations_run = 0usize;
        let hard_iteration_limit = params
            .max_iterations
            .saturating_add(crate::turn_hook::MAX_TERMINAL_CONTINUATION_ITERATIONS);

        'iteration: for iteration in 0..hard_iteration_limit {
            // Apply subscriber writebacks before building the next request.
            let _ =
                apply_turn_hook_writebacks(&mut manager, &mut params, &mut context, &tx, iteration);
            if iteration >= params.max_iterations {
                if terminal_continuations_remaining == 0 {
                    break;
                }
                terminal_continuations_remaining -= 1;
            }
            iterations_run = iteration + 1;
            if cancel.is_cancelled() {
                let _ = tx.send(QueryEvent::Cancelled);
                return None;
            }

            if request_scoped_transient_context {
                let dynamic = params.attachment_poller.as_ref().and_then(|binding| {
                    binding
                        .poller
                        .transient_context_for_query(&binding.session_id, &binding.turn_id)
                });
                params.transient_context_message =
                    merge_transient_context(base_transient_context_message.clone(), dynamic);
            }

            let (estimated_input_tokens, cache_miss_reason) = match compact_before_request(
                &mut manager,
                &mut params,
                &session,
                &tx,
                iteration,
                &mut skip_auto_compact,
                &mut pending_cache_miss_reason,
            )
            .await
            {
                PreTurnCompactFlow::Ran {
                    estimated_input_tokens,
                    cache_miss_reason,
                } => (estimated_input_tokens, cache_miss_reason),
                PreTurnCompactFlow::NextIteration => continue 'iteration,
            };

            let BuiltRequest {
                request,
                request_cache_trace_context,
            } = build_iteration_request(
                self,
                fresh_history,
                &mut manager,
                &mut params,
                &mut context,
                &tx,
                iteration,
                cache_miss_reason,
            );
            announced_tools.remove(RUN_CODE_TOOL);
            extend_announced_tools(&params, &mut announced_tools);

            let accumulator = match open_and_drain_stream(
                &mut manager,
                &mut params,
                &session,
                &client,
                &cancel,
                &tx,
                request,
                &mut context_overflow_retried,
                &mut last_request_failure,
                &mut transient_replay_streak,
                &mut pending_cache_miss_reason,
            )
            .await
            {
                StreamDrainFlow::Ran { accumulator } => accumulator,
                StreamDrainFlow::NextIteration => continue 'iteration,
                StreamDrainFlow::Stop => return None,
            };

            let (message, wants_tools, truncated_tool_uses) = match settle_stream_outcome(
                &mut manager,
                &mut params,
                &mut context,
                &client,
                &tx,
                accumulator,
                iteration,
                &mut announced_tools,
                &mut total_usage,
                &mut stop_reason_final,
                &mut pending_max_token_content,
                &mut automatic_max_token_continuations,
                &mut truncated_tool_call_rounds,
                &mut transient_replay_streak,
                request_scoped_transient_context,
                &mut base_transient_context_message,
                &request_cache_trace_context,
                &mut last_message,
            )
            .await
            {
                StreamOutcomeFlow::Ran {
                    message,
                    wants_tools,
                    truncated_tool_uses,
                } => (message, wants_tools, truncated_tool_uses),
                StreamOutcomeFlow::NextIteration => continue 'iteration,
                StreamOutcomeFlow::Stop => return None,
            };

            match settle_stop_reason(
                &mut manager,
                &mut params,
                &mut context,
                &client,
                &cancel,
                &tx,
                &message,
                iteration,
                wants_tools,
                &mut total_usage,
                &mut stop_reason_final,
                &mut terminal_continuations_remaining,
            )
            .await
            {
                StopReasonFlow::Ran => {}
                StopReasonFlow::NextIteration => continue 'iteration,
                StopReasonFlow::Stop => return None,
            }

            // The classifier sees history through the turn immediately before this
            // assistant response. The current response's narration is model-authored
            // and must not manufacture user intent for its own tool calls. A spawned
            // agent's user turns are its parent's words, not the user's.
            let user_turns = if context.agent_id().is_some() {
                crate::auto_mode_classifier::TranscriptUserTurns::DelegatingAgent
            } else {
                crate::auto_mode_classifier::TranscriptUserTurns::Human
            };
            let auto_mode_classifier_transcript =
                crate::auto_mode_classifier::serialize_auto_mode_transcript_as(
                    manager.messages(),
                    user_turns,
                );

            // Append the assistant message to history before dispatching
            // the tool calls, preserving QueryEngine behavior.
            manager.set_usage_baseline(message.usage.input_tokens);
            manager.push_message(ApiMessage {
                role: Role::Assistant,
                content: message.content.clone(),
            });

            let anchored_promoted = match run_tool_round(
                &engine,
                &mut manager,
                &mut params,
                &mut context,
                &cancel,
                &tx,
                &message,
                &mut announced_tools,
                &truncated_tool_uses,
                &auto_mode_classifier_transcript,
            )
            .await
            {
                ToolRoundFlow::Ran { anchored_promoted } => anchored_promoted,
                ToolRoundFlow::Cancelled => return None,
            };

            match finish_tool_round(
                &mut manager,
                &mut params,
                &mut context,
                &session,
                &client,
                &tx,
                &message,
                iteration,
                wants_tools,
                anchored_promoted,
                estimated_input_tokens,
                request_scoped_transient_context,
                &mut base_transient_context_message,
                &mut skip_auto_compact,
                &mut pending_cache_miss_reason,
            )
            .await
            {
                PostRoundFlow::Ran => {}
                PostRoundFlow::NextIteration => continue 'iteration,
                PostRoundFlow::Reset(reset_messages) => return Some(reset_messages),
            }

            let budget_writeback = tx.dispatch_turn_budget(iteration, params.max_iterations);
            // This phase historically joined history at this exact seam, before
            // queued raw-event hook writes are drained on the next iteration.
            let _ = apply_turn_hook_writeback(
                &mut manager,
                &mut params,
                &mut context,
                &tx,
                iteration + 1,
                budget_writeback,
            );
        }

        // Fell off the iteration cap without terminating.
        if let Some(err) = last_request_failure {
            let _ = tx.send(QueryEvent::Error(err));
        } else if let Some(message) = last_message {
            let _ = tx.send(QueryEvent::IterationLimitReached {
                iterations: iterations_run,
            });
            let _ = tx.send(QueryEvent::Done {
                final_message: message,
                stop_reason: stop_reason_final.unwrap_or(StopReason::EndTurn),
                total_usage,
            });
        } else {
            let _ = tx.send(QueryEvent::Error(
                "query loop exited without producing any assistant message".into(),
            ));
        }
        None
    }
}

pub(super) async fn dispatch_tool_use(
    engine: &Arc<Engine>,
    tool_use: &ToolUseBlock,
    context: &ToolContext,
    tx: &QueryEventSender,
    policy: PolicySources,
) -> Result<Value, ToolErrorPresentation> {
    let (child_context, mut progress_rx) = context.with_progress(tool_use.id.clone());
    let tool_name = tool_use.name.clone();
    let tool_use_id = tool_use.id.clone();
    let engine = Arc::clone(engine);
    let invoke_name = tool_use.name.clone();
    let mut invoke_input = tool_use.input.clone();
    match crate::hooks::run_pre_tool_use_hooks(
        &policy,
        &invoke_name,
        invoke_input.clone(),
        &tool_use_id,
    )
    .await
    {
        PreToolUseDecision::Continue { input } => {
            invoke_input = input;
        }
        PreToolUseDecision::Blocked { reason } => {
            return Err(ToolErrorPresentation::same("hook_blocked", reason));
        }
    }
    let policy_for_result = policy.clone();
    // The input the tool actually ran with, after `PreToolUse` rewrites: the
    // annotation phase reads the tool's path out of it, so it has to be the
    // dispatched one and not `tool_use.input`.
    let annotation_input = invoke_input.clone();
    let dispatched_input = invoke_input.clone();
    let invoke_tool_use_id = tool_use_id.clone();
    let invoke_context = child_context.clone();
    let mut invoke: BoxFuture<'static, Result<Value, ToolErrorPresentation>> = async move {
        let result = engine
            .invoke_tool(&invoke_name, invoke_input, &invoke_context)
            .await
            .map_err(|err: ToolError| err.presentation());
        match result {
            Ok(output) => Ok(crate::hooks::run_post_tool_use_hooks(
                &policy_for_result,
                &invoke_name,
                dispatched_input,
                output,
                &invoke_tool_use_id,
            )
            .await),
            Err(error) => {
                crate::hooks::run_post_tool_use_failure_hooks(
                    &policy_for_result,
                    &invoke_name,
                    dispatched_input,
                    &invoke_tool_use_id,
                    &error.model_message,
                )
                .await;
                Err(error)
            }
        }
    }
    .boxed();

    loop {
        tokio::select! {
            result = &mut invoke => {
                while let Ok(progress) = progress_rx.try_recv() {
                    let _ = tx.send(QueryEvent::ToolDispatchProgress {
                        tool_use_id: tool_use_id.clone(),
                        name: tool_name.clone(),
                        progress,
                    });
                }
                // Let subscribers add fields to a successful result before it
                // becomes the `ToolDispatchResult` event: the transcript and
                // the UI read the annotation off the same object the tool
                // returned, so annotating after the send would be invisible.
                let mut result = result;
                if let Ok(output) = result.as_mut() {
                    tx.annotate_tool_result(
                        &tool_name,
                        &annotation_input,
                        context.cwd(),
                        output,
                    );
                }
                return result;
            }
            Some(progress) = progress_rx.recv() => {
                let _ = tx.send(QueryEvent::ToolDispatchProgress {
                    tool_use_id: tool_use_id.clone(),
                    name: tool_name.clone(),
                    progress,
                });
            }
        }
    }
}
#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use async_trait::async_trait;
    use futures_util::Stream;
    use rebon_api::{ModelError, ModelResult, StreamEventStream};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context as TaskContext, Poll};
    use std::time::Duration;

    struct PendingProviderStream {
        polled: Arc<Notify>,
        dropped: Arc<AtomicBool>,
    }

    impl Stream for PendingProviderStream {
        type Item = ModelResult<StreamEvent>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
            self.polled.notify_one();
            Poll::Pending
        }
    }

    impl Drop for PendingProviderStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct PendingProviderStreamClient {
        requests: Arc<AtomicUsize>,
        stream_polled: Arc<Notify>,
        stream_dropped: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ModelClient for PendingProviderStreamClient {
        fn provider_name(&self) -> &'static str {
            "pending-provider-stream-test"
        }

        async fn create_message_stream(
            &self,
            _request: CreateMessageRequest,
        ) -> ModelResult<StreamEventStream> {
            self.requests.fetch_add(1, Ordering::AcqRel);
            Ok(Box::pin(PendingProviderStream {
                polled: self.stream_polled.clone(),
                dropped: self.stream_dropped.clone(),
            }))
        }
    }

    struct ControllerTransientClient {
        requests: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelClient for ControllerTransientClient {
        fn provider_name(&self) -> &'static str {
            "controller-transient-test"
        }

        async fn create_message_stream(
            &self,
            _request: CreateMessageRequest,
        ) -> ModelResult<StreamEventStream> {
            let request = self.requests.fetch_add(1, Ordering::AcqRel);
            assert_eq!(request, 0, "controller issued an unintended replay request");
            Err(ModelError::transient(
                "force TurnControlPlugin transient replay",
            ))
        }
    }

    async fn expect_cancelled(rx: &mut mpsc::UnboundedReceiver<QueryEvent>, boundary: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match rx.recv().await {
                    Some(QueryEvent::Cancelled) => break,
                    Some(_) => {}
                    None => panic!("query ended without cancellation at {boundary}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("cancellation did not promptly interrupt {boundary}"));
    }

    #[tokio::test]
    async fn cancellation_drops_controller_owned_pending_provider_stream() {
        let requests = Arc::new(AtomicUsize::new(0));
        let stream_polled = Arc::new(Notify::new());
        let stream_dropped = Arc::new(AtomicBool::new(false));
        let client: Arc<dyn ModelClient> = Arc::new(PendingProviderStreamClient {
            requests: requests.clone(),
            stream_polled: stream_polled.clone(),
            stream_dropped: stream_dropped.clone(),
        });
        let cancel = CancelToken::new();
        let mut rx = run_query(
            Arc::new(Engine::new()),
            SessionHandle::new(client),
            QueryParams::new("mock", vec![ApiMessage::user_text("start")]),
            ToolContext::new(),
            cancel.clone(),
        );

        tokio::time::timeout(Duration::from_secs(1), stream_polled.notified())
            .await
            .expect("TurnControlPlugin did not poll the pending provider stream");
        assert_eq!(requests.load(Ordering::Acquire), 1);
        assert!(!stream_dropped.load(Ordering::Acquire));

        cancel.cancel();
        expect_cancelled(&mut rx, "the pending provider stream").await;

        assert_eq!(
            requests.load(Ordering::Acquire),
            1,
            "stream cancellation must not issue another request"
        );
        assert!(
            stream_dropped.load(Ordering::Acquire),
            "controller cancellation must drop the pending provider stream"
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_controller_transient_replay_backoff() {
        let requests = Arc::new(AtomicUsize::new(0));
        let client: Arc<dyn ModelClient> = Arc::new(ControllerTransientClient {
            requests: requests.clone(),
        });
        let session = SessionHandle::new(client);
        let params = QueryParams::new("mock", vec![ApiMessage::user_text("start")]);
        let cancel = CancelToken::new();
        let (raw_tx, mut rx) = mpsc::unbounded_channel();
        let tx = QueryEventSender::new(raw_tx, &params.turn_hooks);
        let plugin = TurnControlPlugin {
            engine: Arc::new(Engine::new()),
            session,
            params,
            context: ToolContext::new(),
            cancel: cancel.clone(),
            tx,
        };
        let backoff_entered = Arc::new(Notify::new());
        let probe = TransientReplayBackoffProbe {
            entered: backoff_entered.clone(),
            delay: Duration::from_secs(60),
        };
        let controller = tokio::spawn(
            TRANSIENT_REPLAY_BACKOFF_PROBE.scope(probe, async move { plugin.run().await }),
        );

        tokio::time::timeout(Duration::from_secs(1), backoff_entered.notified())
            .await
            .expect("TurnControlPlugin did not enter its transient replay backoff");
        assert_eq!(requests.load(Ordering::Acquire), 1);

        cancel.cancel();
        expect_cancelled(&mut rx, "TurnControlPlugin transient replay backoff").await;
        tokio::time::timeout(Duration::from_secs(1), controller)
            .await
            .expect("controller did not finish promptly after backoff cancellation")
            .expect("controller task panicked");

        assert_eq!(
            requests.load(Ordering::Acquire),
            1,
            "backoff cancellation must not replay the request"
        );
    }
}
