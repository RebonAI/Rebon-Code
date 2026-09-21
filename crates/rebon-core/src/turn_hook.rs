//! Ordered, disposable subscriptions to a query turn.
//!
//! The runtime snapshots subscribers at query start, invokes synchronous
//! borrowed phases in stable order, and commits owned writebacks only at
//! deterministic controller seams. Disposed or panicking hooks are isolated.

use std::any::Any;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use futures_util::FutureExt;

use rebon_api::{
    AssistantMessage, CacheMissReason, CacheTraceContext, CreateMessageRequest,
    Message as ApiMessage, RequestShapeTrace, Usage,
};
use rebon_kernel::{Context, Disposer, KernelError, Service};

use crate::query::{QueryEvent, QueryEventObserver, QueryParams};

pub const TURN_HOOK_SEAT_SERVICE: &str = "turn-hooks";
pub const CACHE_TRACE_HOOK_ID: &str = "core/cache-trace";
pub const TURN_BUDGET_WARNING_HOOK_ID: &str = "core/turn-budget-warning";
pub const ATTACHMENT_INJECTION_HOOK_ID: &str = "core/attachment-injection";
pub const TERMINAL_ATTACHMENT_HOOK_ID: &str = "core/terminal-attachments";
pub const TASK_TURN_RECONCILIATION_HOOK_ID: &str = "core/task-turn-reconciliation";

const TURN_BUDGET_WARNING_REMAINING: usize = 10;

/// Hard stop covers attachment, task, and query-local terminal continuations.
pub(crate) const MAX_TERMINAL_CONTINUATION_ITERATIONS: usize = 5;

const TERMINAL_ATTACHMENT_FOLLOWUP_ITERATIONS: usize = 1;
const TASK_RECONCILIATION_FOLLOWUP_ITERATIONS: usize = 2;

/// Built-in terminal order; query-local policy hooks normally run last.
const TERMINAL_ATTACHMENT_ORDER: Order = Order::new(-100);
const TASK_RECONCILIATION_ORDER: Order = Order::new(-50);

/// A subscriber's position in a hook phase. Lower values run first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Order(i32);

impl Order {
    pub const FIRST: Self = Self(-1_000);
    pub const NORMAL: Self = Self(0);
    pub const LAST: Self = Self(1_000);

    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i32 {
        self.0
    }
}

/// Per-subscriber mutable state, reset at the start of each drive.
#[derive(Default)]
pub struct TurnHookState(Option<Box<dyn Any + Send>>);

impl TurnHookState {
    /// Borrow this subscriber's state, creating or retyping it as needed.
    pub fn get_mut<T: Default + Send + 'static>(&mut self) -> &mut T {
        let needs_init = match self.0.as_ref() {
            Some(state) => !state.is::<T>(),
            None => true,
        };
        if needs_init {
            self.0 = Some(Box::<T>::default());
        }
        self.0
            .as_mut()
            .expect("state slot was just populated")
            .downcast_mut::<T>()
            .expect("state slot holds the type it was just populated with")
    }
}

type ParamUpdate = Box<dyn FnOnce(&mut QueryParams) + Send>;

/// Writebacks requested by one subscriber invocation.
#[derive(Default)]
pub struct TurnHookContext {
    pub(crate) history: Vec<ApiMessage>,
    param_updates: Vec<ParamUpdate>,
    pub(crate) injected_attachments: Vec<ApiMessage>,
    pub(crate) coordinator_report_paths: Vec<std::path::PathBuf>,
    pub(crate) continue_followups: Option<usize>,
}

impl TurnHookContext {
    /// Append one message before the next model request.
    pub fn append_history(&mut self, message: ApiMessage) {
        self.history.push(message);
    }

    /// Append and announce one injected attachment before the next request.
    pub fn append_injected_attachment(&mut self, message: ApiMessage) {
        self.injected_attachments.push(message);
    }

    /// Announce report paths the current tool context should be able to read.
    pub fn extend_coordinator_report_paths(
        &mut self,
        paths: impl IntoIterator<Item = std::path::PathBuf>,
    ) {
        self.coordinator_report_paths.extend(paths);
    }

    /// Update the optional portion of the live query parameters.
    pub fn update_params<F>(&mut self, update: F)
    where
        F: FnOnce(&mut QueryParams) + Send + 'static,
    {
        self.param_updates.push(Box::new(update));
    }

    /// Ask an otherwise terminal response to run another model iteration,
    /// reserving one follow-up iteration past the turn's iteration cap.
    pub fn request_continue(&mut self) {
        self.request_continue_reserving(1);
    }

    /// Continue and reserve `followups` iterations past the normal cap.
    pub fn request_continue_reserving(&mut self, followups: usize) {
        let reserved = self.continue_followups.unwrap_or(0).max(followups);
        self.continue_followups = Some(reserved);
    }

    fn append(&mut self, mut other: Self) {
        self.history.append(&mut other.history);
        self.param_updates.append(&mut other.param_updates);
        self.injected_attachments
            .append(&mut other.injected_attachments);
        self.coordinator_report_paths
            .append(&mut other.coordinator_report_paths);
        if let Some(followups) = other.continue_followups {
            self.request_continue_reserving(followups);
        }
    }

    pub(crate) fn apply_to_params(mut self, params: &mut QueryParams) -> Self {
        for update in self.param_updates.drain(..) {
            update(params);
        }
        self
    }
}

/// A post-tool-round iteration-budget snapshot delivered synchronously.
///
/// The dispatcher lends this non-cloneable snapshot only for the synchronous
/// callback; it contains no live engine references and cannot cross an await.
#[derive(Debug, PartialEq, Eq)]
pub struct TurnBudgetEvent {
    pub iteration: usize,
    pub max_iterations: usize,
}

impl TurnBudgetEvent {
    #[cfg(test)]
    pub(crate) const fn after_tool_round(iteration: usize, max_iterations: usize) -> Self {
        Self {
            iteration,
            max_iterations,
        }
    }

    pub fn remaining_iterations(&self) -> usize {
        self.max_iterations.saturating_sub(self.iteration + 1)
    }
}

/// A borrowed cache-trace snapshot delivered synchronously to turn hooks.
/// The lifetime prevents a subscriber from retaining request state across an await.
#[derive(Debug)]
pub enum CacheTraceEvent<'a> {
    SessionBasePromptCache {
        stable_base_system: bool,
        cache_hits: &'a [bool],
    },
    RequestBuilt {
        request: &'a CreateMessageRequest,
        runtime_context_message: Option<&'a str>,
        transient_context_message: Option<&'a str>,
        cache_miss_reason: CacheMissReason,
    },
    RequestUsage {
        model: &'a str,
        usage: &'a Usage,
        cache_trace_context: Option<&'a CacheTraceContext>,
    },
}

/// A completed tool round at the historical post-dispatch seam.
///
/// The assistant message keeps tool uses in provider order even though their
/// calls ran concurrently. Individual tool failures are still a completed
/// round; cancellation is not, so the loop does not dispatch this phase after
/// an interrupted batch. All handles are borrowed only while subscribers build
/// owned futures and cannot cross an await.
pub struct ToolRoundHookEvent<'a> {
    pub message: &'a AssistantMessage,
    pub iteration: usize,
    attachment_poller: Option<&'a crate::query::AttachmentPollerBinding>,
    extensions: &'a rebon_tool::Extensions,
}

impl<'a> ToolRoundHookEvent<'a> {
    /// Borrow this turn's state for one feature, if the host wired it.
    ///
    /// The seam a subscriber outside this crate reaches its own session state
    /// through: the bag is the turn's, the type is the subscriber's, and the
    /// engine never learns either. `None` means the host wired nothing, which
    /// is a subscriber that has nothing to do rather than an error.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }

    #[cfg(test)]
    pub(crate) fn completed(
        message: &'a AssistantMessage,
        iteration: usize,
        params: &'a QueryParams,
    ) -> Self {
        Self {
            message,
            iteration,
            attachment_poller: params.attachment_poller.as_ref(),
            extensions: &params.extensions,
        }
    }
}

/// One dispatched tool call's outcome, at the sequential seam that builds the
/// `tool_result` blocks.
///
/// Tool calls run concurrently but their results are folded back in provider
/// order, so subscribers observe them in the order the model wrote them and
/// may rely on it. A cancelled round never reaches this phase.
pub struct ToolResultHookEvent<'a> {
    pub tool_name: &'a str,
    pub output: Option<&'a serde_json::Value>,
    attachment_poller: Option<&'a crate::query::AttachmentPollerBinding>,
}

impl<'a> ToolResultHookEvent<'a> {
    #[cfg(test)]
    pub(crate) fn completed(
        tool_name: &'a str,
        output: Option<&'a serde_json::Value>,
        params: &'a QueryParams,
    ) -> Self {
        Self {
            tool_name,
            output,
            attachment_poller: params.attachment_poller.as_ref(),
        }
    }

    pub fn succeeded(&self) -> bool {
        self.output.is_some()
    }
}

/// One tool call that just succeeded, before anything has seen its output.
///
/// The seam is inside the dispatch task, between the tool returning and the
/// `ToolDispatchResult` event going out, because the annotation has to reach
/// the transcript and the UI as part of the result rather than after it.
///
/// It exists so a feature can annotate a *core* tool's result without the
/// core tool knowing the feature: the memory plugin's "Memory updated in …"
/// line used to be three copies of an `is_auto_mem_path` branch inside
/// `Write`, `Edit` and `MultiEdit`, which is why `rebon-tool` depended on the
/// memory crate.
pub struct ToolAnnotationHookEvent<'a> {
    /// Registered name of the tool that ran.
    pub tool_name: &'a str,
    /// The input it ran with, after any `PreToolUse` hook rewrote it — so a
    /// subscriber reading the tool's `file_target_field` sees the path the
    /// tool actually acted on.
    pub input: &'a serde_json::Value,
    /// The turn's working directory, when the tool context carries one.
    pub cwd: Option<&'a str>,
}

/// Transactional writeback for one annotation subscriber.
///
/// A subscriber may only *add* fields to the tool's output object, never
/// rewrite what the tool returned: a hook that could replace the result would
/// be a second implementation of the tool. Fields are merged in subscriber
/// order after the phase, and a panicking subscriber's fields are discarded
/// with the rest of its work.
#[derive(Default)]
pub struct ToolAnnotationHookContext {
    fields: Vec<(String, serde_json::Value)>,
}

impl ToolAnnotationHookContext {
    /// Add one field to the completed tool's output object.
    pub fn annotate(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.fields.push((key.into(), value));
    }

    /// The fields asked for so far, in the order they were added — what the
    /// runtime is about to merge. A subscriber's own tests assert on this
    /// rather than re-implementing the merge.
    pub fn fields(&self) -> &[(String, serde_json::Value)] {
        &self.fields
    }
}

/// The terminal candidate for one turn, before `Done` is emitted.
///
/// Subscribers form an ordered waterfall and the first one that asks to
/// continue ends the phase, so a later subscriber never observes a turn an
/// earlier one already decided to extend. That is exactly the historical seam
/// — eager attachments, then task reconciliation, then query-local terminal
/// policy hooks — with one mechanism instead of three inline branches.
pub struct TurnEndHookEvent<'a> {
    pub message: &'a AssistantMessage,
    pub iteration: usize,
    pub max_iterations: usize,
    pub history: &'a [ApiMessage],
    pub task_list_id: &'a str,
    attachment_poller: Option<&'a crate::query::AttachmentPollerBinding>,
}

impl<'a> TurnEndHookEvent<'a> {
    #[cfg(test)]
    pub(crate) fn terminal_candidate(
        message: &'a AssistantMessage,
        iteration: usize,
        history: &'a [ApiMessage],
        task_list_id: &'a str,
        params: &'a QueryParams,
    ) -> Self {
        Self {
            message,
            iteration,
            max_iterations: params.max_iterations,
            history,
            task_list_id,
            attachment_poller: params.attachment_poller.as_ref(),
        }
    }
}

/// Owned asynchronous work returned by a synchronous tool-round subscriber.
///
/// The `'static` bound is deliberate: a subscriber must copy what it needs out
/// of [`ToolRoundHookEvent`] before returning, so no live turn borrow can be
/// retained across an await.
pub type TurnHookFuture = Pin<Box<dyn Future<Output = TurnHookContext> + Send + 'static>>;

/// One attachment poll the loop is about to make.
///
/// The loop owns the iteration budget and therefore decides *whether* to ask;
/// a subscriber decides *what* the ask produces.
pub struct AttachmentPollHookEvent<'a> {
    pub phase: crate::query::AttachmentPollPhase,
    pub next_iteration: u64,
    pub history: &'a [ApiMessage],
    attachment_poller: Option<&'a crate::query::AttachmentPollerBinding>,
}

impl<'a> AttachmentPollHookEvent<'a> {
    #[cfg(test)]
    pub(crate) fn scheduled(
        phase: crate::query::AttachmentPollPhase,
        next_iteration: u64,
        history: &'a [ApiMessage],
        params: &'a QueryParams,
    ) -> Self {
        Self {
            phase,
            next_iteration,
            history,
            attachment_poller: params.attachment_poller.as_ref(),
        }
    }
}

/// Poll one attachment producer and announce what it returned.
///
/// Returns whether anything the poller produced can carry the turn: an
/// attachment whose model projection is empty is still announced to clients,
/// but gives the next request nothing to answer.
fn collect_attachments(
    binding: &crate::query::AttachmentPollerBinding,
    request: crate::query::AttachmentPollRequest<'_>,
    history: &[ApiMessage],
    context: &mut TurnHookContext,
) -> bool {
    let injected = binding.poller.poll(request);
    context.extend_coordinator_report_paths(
        binding
            .poller
            .take_coordinator_report_paths_for_query(&binding.session_id, &binding.turn_id),
    );
    let mut carries_the_turn = false;
    for attachment in injected {
        if crate::query::attachment_repeats_history(history, &attachment) {
            tracing::debug!("skipping attachment already present in replayed history");
            continue;
        }
        carries_the_turn |= !crate::query::model_message_for_attachment(&attachment)
            .content
            .is_empty();
        context.append_injected_attachment(attachment);
    }
    carries_the_turn
}

/// Turn a scheduled poll into announced attachments.
///
/// This is the loop's attachment-poll contract: poll before the first
/// request and after every completed tool round, and let the producer decide
/// what a given phase and iteration is worth injecting.
struct AttachmentInjectionHook;

impl TurnHook for AttachmentInjectionHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

    fn on_attachment_poll(
        &self,
        event: &AttachmentPollHookEvent<'_>,
        context: &mut TurnHookContext,
    ) {
        let Some(binding) = event.attachment_poller else {
            return;
        };
        collect_attachments(
            binding,
            binding.request(event.next_iteration, event.phase),
            event.history,
            context,
        );
    }
}

/// Deliver an eager attachment poll instead of ending the turn.
///
/// The poll runs only while the turn still has a base iteration to spend, so a
/// response that already sits on the cap is not extended by one. An attachment
/// whose model projection is empty is announced but cannot carry the turn: it
/// gives the next request nothing to answer.
struct TerminalAttachmentHook;

impl TurnHook for TerminalAttachmentHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

    fn on_turn_end(
        &self,
        event: &TurnEndHookEvent<'_>,
        context: &mut TurnHookContext,
        _state: &mut TurnHookState,
    ) {
        if event.iteration >= event.max_iterations {
            return;
        }
        let message = event.message;
        let Some(binding) = event.attachment_poller else {
            return;
        };
        let mut poll = TurnHookContext::default();
        let carries_the_turn = collect_attachments(
            binding,
            binding.request(
                (event.iteration as u64).saturating_add(1),
                crate::query::AttachmentPollPhase::Eager,
            ),
            event.history,
            &mut poll,
        );
        if carries_the_turn {
            // The assistant response has to precede the attachments it is
            // being answered against, so it is written before the poll's own
            // messages are merged in.
            context.append_history(ApiMessage {
                role: rebon_api::Role::Assistant,
                content: message.content.clone(),
            });
        }
        context.append(poll);
        if carries_the_turn {
            context.request_continue_reserving(TERMINAL_ATTACHMENT_FOLLOWUP_ITERATIONS);
        }
    }
}

/// Everything the turn owes the task list.
///
/// The tracker records which tasks the turn touched, resets the attachment
/// poller's reminder throttle for the round that touched them, and asks for one
/// more exchange when the turn would end with a task it opened still open. The
/// reminder is deliberately once per drive: repeating it every terminal
/// candidate would trap a turn that has decided the work is blocked.
#[derive(Default)]
struct TaskTurnTrackerState {
    touched_ids: std::collections::BTreeSet<String>,
    reminder_sent: bool,
}

struct TaskTurnReconciliationHook;

impl TaskTurnReconciliationHook {
    fn touched_task_id<'a>(tool_name: &str, output: &'a serde_json::Value) -> Option<&'a str> {
        match tool_name {
            "TaskCreate" => output
                .get("task")
                .and_then(|task| task.get("id"))
                .and_then(serde_json::Value::as_str),
            "TaskUpdate"
                if output.get("success").and_then(serde_json::Value::as_bool) == Some(true) =>
            {
                output.get("taskId").and_then(serde_json::Value::as_str)
            }
            _ => None,
        }
    }

    fn is_task_tool(tool_name: &str) -> bool {
        tool_name == "TaskCreate" || tool_name == "TaskUpdate"
    }
}

impl TurnHook for TaskTurnReconciliationHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

    fn on_tool_result(&self, event: &ToolResultHookEvent<'_>, state: &mut TurnHookState) {
        if let Some(binding) = event.attachment_poller {
            binding
                .poller
                .notify_plan_mode_tool(event.tool_name, event.succeeded(), event.output);
        }
        let Some(output) = event.output else {
            return;
        };
        let Some(task_id) = Self::touched_task_id(event.tool_name, output) else {
            return;
        };
        state
            .get_mut::<TaskTurnTrackerState>()
            .touched_ids
            .insert(task_id.to_string());
    }

    fn on_tool_round(&self, event: &ToolRoundHookEvent<'_>) -> Option<TurnHookFuture> {
        let binding = event.attachment_poller?;
        if !event
            .message
            .tool_uses()
            .any(|tool_use| Self::is_task_tool(&tool_use.name))
        {
            return None;
        }
        binding.poller.notify_task_tool_used(event.iteration as u64);
        None
    }

    fn on_turn_end(
        &self,
        event: &TurnEndHookEvent<'_>,
        context: &mut TurnHookContext,
        state: &mut TurnHookState,
    ) {
        let message = event.message;
        if !matches!(
            message.stop_reason,
            None | Some(rebon_api::StopReason::EndTurn)
        ) {
            return;
        }
        let tracker = state.get_mut::<TaskTurnTrackerState>();
        if tracker.reminder_sent || tracker.touched_ids.is_empty() {
            return;
        }

        let mut unfinished = Vec::new();
        for task_id in &tracker.touched_ids {
            match rebon_tool::tasks::get_task(event.task_list_id, task_id) {
                Ok(Some(task)) if task.status != rebon_tool::tasks::TaskListStatus::Completed => {
                    unfinished.push((task.id, task.status));
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        task_list_id = %event.task_list_id,
                        task_id,
                        error = %err,
                        "failed to inspect touched task during turn reconciliation"
                    );
                }
            }
        }
        if unfinished.is_empty() {
            return;
        }

        tracker.reminder_sent = true;
        let tasks = unfinished
            .into_iter()
            .map(|(id, status)| format!("- #{}: {}", id, status))
            .collect::<Vec<_>>()
            .join("\n");
        tracing::info!("task reconciliation gate fired — continuing loop");
        context.append_history(ApiMessage {
            role: rebon_api::Role::Assistant,
            content: message.content.clone(),
        });
        context.append_history(ApiMessage::user_text(format!(
            "<system-reminder>\nBefore finishing, reconcile the tasks touched in this turn:\n{tasks}\nMark completed work with TaskUpdate. If work remains blocked or incomplete, leave its status open and explain the blocker in the final response. This reminder will not repeat.\n</system-reminder>"
        )));
        context.request_continue_reserving(TASK_RECONCILIATION_FOLLOWUP_ITERATIONS);
    }
}

struct TurnBudgetWarningHook;

impl TurnHook for TurnBudgetWarningHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

    fn on_turn_budget(&self, event: &TurnBudgetEvent, context: &mut TurnHookContext) {
        let remaining = event.remaining_iterations();
        if remaining != TURN_BUDGET_WARNING_REMAINING {
            return;
        }
        context.append_history(ApiMessage::user_text(format!(
            "[SYSTEM: You are approaching the iteration limit. \
             You have {} iterations remaining out of {}. \
             Please wrap up your current work and provide a final response.]",
            remaining, event.max_iterations
        )));
    }
}

struct CacheTraceHook;

impl TurnHook for CacheTraceHook {
    fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

    fn on_cache_trace(&self, event: &CacheTraceEvent<'_>) {
        match event {
            CacheTraceEvent::SessionBasePromptCache {
                stable_base_system: true,
                cache_hits,
            } if !cache_hits.is_empty() => {
                let hits = cache_hits.iter().filter(|hit| **hit).count();
                let misses = cache_hits.len().saturating_sub(hits);
                tracing::info!(
                    target: "rebon_cache_trace",
                    provider = "engine",
                    base_prompt_cache_hits = hits,
                    base_prompt_cache_misses = misses,
                    "session_base_prompt_cache"
                );
            }
            CacheTraceEvent::RequestBuilt {
                request,
                runtime_context_message,
                transient_context_message,
                cache_miss_reason,
            } => {
                let mut shape = RequestShapeTrace::from_request(request);
                if crate::query::stable_base_system_enabled() {
                    let dynamic_context_hash_input =
                        [*runtime_context_message, *transient_context_message]
                            .into_iter()
                            .flatten()
                            .filter(|context| !context.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                    shape.dynamic_context_hash = if dynamic_context_hash_input.is_empty() {
                        None
                    } else {
                        Some(rebon_api::stable_hash_str(&dynamic_context_hash_input))
                    };
                }
                let trace = request.cache_trace_context.as_ref();
                tracing::info!(
                    target: "rebon_cache_trace",
                    provider = "engine",
                    model = %request.model,
                    model_hash = %shape.model_hash,
                    system_hash = %shape.system_hash,
                    tools_hash = %trace
                        .and_then(|trace| trace.tools_hash.as_deref())
                        .unwrap_or(&shape.tools_hash),
                    schema_hash = trace.and_then(|trace| trace.schema_hash.as_deref()),
                    shared_preamble_hash = trace.and_then(|trace| trace.shared_preamble_hash.as_deref()),
                    profile_preamble_hash = trace.and_then(|trace| trace.profile_preamble_hash.as_deref()),
                    capsule_hash = trace.and_then(|trace| trace.capsule_hash.as_deref()),
                    task_hash = trace.and_then(|trace| trace.task_hash.as_deref()),
                    messages_prefix_hash = %shape.messages_prefix_hash,
                    reasoning_hash = %shape.reasoning_hash,
                    dynamic_context_hash = shape.dynamic_context_hash.as_deref(),
                    context_policy = trace.and_then(|trace| trace.context_policy.as_deref()),
                    tokens_before_capsule = trace.and_then(|trace| trace.tokens_before_capsule),
                    tokens_before_task = trace.and_then(|trace| trace.tokens_before_task),
                    cross_run_cache_eligible = trace.and_then(|trace| trace.cross_run_cache_eligible),
                    same_dispatch_cache_eligible = trace.and_then(|trace| trace.same_dispatch_cache_eligible),
                    previous_response_id_present = trace.and_then(|trace| trace.previous_response_id_present),
                    prompt_cache_key = trace.and_then(|trace| trace.prompt_cache_key.as_deref()),
                    prompt_cache_retention = trace.and_then(|trace| trace.prompt_cache_retention.as_deref()),
                    api_path = trace.and_then(|trace| trace.api_path.as_deref()),
                    cache_miss_reason = cache_miss_reason.as_str(),
                    "request_shape_trace"
                );
            }
            CacheTraceEvent::RequestUsage {
                model,
                usage,
                cache_trace_context,
            } => {
                let cached_tokens = usage
                    .prompt_cache_hit_tokens
                    .saturating_add(usage.cache_read_input_tokens);
                let prompt_tokens = usage.billed_input_tokens();
                let cached_ratio = if prompt_tokens > 0 {
                    cached_tokens as f64 / prompt_tokens as f64
                } else {
                    0.0
                };
                tracing::info!(
                    target: "rebon_cache_trace",
                    provider = "engine",
                    model = %model,
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    cache_read_input_tokens = usage.cache_read_input_tokens,
                    cache_creation_input_tokens = usage.cache_creation_input_tokens,
                    prompt_cache_hit_tokens = usage.prompt_cache_hit_tokens,
                    prompt_cache_miss_tokens = usage.prompt_cache_miss_tokens,
                    prompt_tokens,
                    cached_tokens,
                    cached_ratio,
                    previous_response_id_present = cache_trace_context.and_then(|trace| trace.previous_response_id_present),
                    prompt_cache_key = cache_trace_context.and_then(|trace| trace.prompt_cache_key.as_deref()),
                    prompt_cache_retention = cache_trace_context.and_then(|trace| trace.prompt_cache_retention.as_deref()),
                    api_path = cache_trace_context.and_then(|trace| trace.api_path.as_deref()),
                    context_policy = cache_trace_context.and_then(|trace| trace.context_policy.as_deref()),
                    tools_hash = cache_trace_context.and_then(|trace| trace.tools_hash.as_deref()),
                    schema_hash = cache_trace_context.and_then(|trace| trace.schema_hash.as_deref()),
                    shared_preamble_hash = cache_trace_context.and_then(|trace| trace.shared_preamble_hash.as_deref()),
                    profile_preamble_hash = cache_trace_context.and_then(|trace| trace.profile_preamble_hash.as_deref()),
                    capsule_hash = cache_trace_context.and_then(|trace| trace.capsule_hash.as_deref()),
                    task_hash = cache_trace_context.and_then(|trace| trace.task_hash.as_deref()),
                    tokens_before_capsule = cache_trace_context.and_then(|trace| trace.tokens_before_capsule),
                    tokens_before_task = cache_trace_context.and_then(|trace| trace.tokens_before_task),
                    cross_run_cache_eligible = cache_trace_context.and_then(|trace| trace.cross_run_cache_eligible),
                    same_dispatch_cache_eligible = cache_trace_context.and_then(|trace| trace.same_dispatch_cache_eligible),
                    "request_usage_trace"
                );
            }
            CacheTraceEvent::SessionBasePromptCache { .. } => {}
        }
    }
}

/// One ordered subscriber.
pub trait TurnHook: Send + Sync {
    /// Observe every raw query event.
    fn on_event(&self, event: &QueryEvent, context: &mut TurnHookContext);

    /// Produce the messages one scheduled attachment poll injects.
    fn on_attachment_poll(
        &self,
        _event: &AttachmentPollHookEvent<'_>,
        _context: &mut TurnHookContext,
    ) {
    }

    /// Observe one completed tool call in provider order, while the round's
    /// `tool_result` blocks are still being assembled.
    fn on_tool_result(&self, _event: &ToolResultHookEvent<'_>, _state: &mut TurnHookState) {}

    /// Add fields to one succeeding tool call's output, before the result
    /// reaches the model, the transcript or the UI. Default: add nothing.
    fn annotate_tool_result(
        &self,
        _event: &ToolAnnotationHookEvent<'_>,
        _context: &mut ToolAnnotationHookContext,
    ) {
    }

    /// Observe a completed tool round at the pre-attachment seam. The callback
    /// itself is synchronous and may return only owned asynchronous work.
    fn on_tool_round(&self, _event: &ToolRoundHookEvent<'_>) -> Option<TurnHookFuture> {
        None
    }

    /// Observe one cache-trace seam using an immutable, synchronous snapshot.
    fn on_cache_trace(&self, _event: &CacheTraceEvent<'_>) {}

    /// Observe the post-tool-round iteration budget. This phase runs only when
    /// the round reached the historical wind-down seam and is about to advance.
    fn on_turn_budget(&self, _event: &TurnBudgetEvent, _context: &mut TurnHookContext) {}

    /// Decide whether an otherwise terminal response ends the turn.
    ///
    /// The phase stops at the first subscriber that calls
    /// [`TurnHookContext::request_continue`], so a subscriber that declines
    /// must leave the context untouched.
    fn on_turn_end(
        &self,
        _event: &TurnEndHookEvent<'_>,
        _context: &mut TurnHookContext,
        _state: &mut TurnHookState,
    ) {
    }
}

impl<F> TurnHook for F
where
    F: Fn(&QueryEvent, &mut TurnHookContext) + Send + Sync,
{
    fn on_event(&self, event: &QueryEvent, context: &mut TurnHookContext) {
        self(event, context);
    }
}

#[derive(Clone)]
pub(crate) struct HookEntry {
    id: String,
    order: Order,
    active: Arc<AtomicBool>,
    hook: Arc<dyn TurnHook>,
}

/// Typed definition for the kernel's `turn-hooks` seat.
pub struct TurnHookSeatService;

impl Service for TurnHookSeatService {
    type Interface = TurnHookSeat;
    const NAME: &'static str = TURN_HOOK_SEAT_SERVICE;
}

/// Process-level registry of ordered turn subscribers.
pub struct TurnHookSeat {
    entries: RwLock<Vec<HookEntry>>,
}

impl TurnHookSeat {
    pub fn new() -> Arc<Self> {
        let seat = Arc::new(Self {
            entries: RwLock::new(Vec::new()),
        });
        let builtins: [(&str, Order, Arc<dyn TurnHook>); 5] = [
            (CACHE_TRACE_HOOK_ID, Order::NORMAL, Arc::new(CacheTraceHook)),
            (
                TURN_BUDGET_WARNING_HOOK_ID,
                Order::NORMAL,
                Arc::new(TurnBudgetWarningHook),
            ),
            (
                ATTACHMENT_INJECTION_HOOK_ID,
                TERMINAL_ATTACHMENT_ORDER,
                Arc::new(AttachmentInjectionHook),
            ),
            (
                TERMINAL_ATTACHMENT_HOOK_ID,
                TERMINAL_ATTACHMENT_ORDER,
                Arc::new(TerminalAttachmentHook),
            ),
            (
                TASK_TURN_RECONCILIATION_HOOK_ID,
                TASK_RECONCILIATION_ORDER,
                Arc::new(TaskTurnReconciliationHook),
            ),
        ];
        for (id, order, hook) in builtins {
            drop(
                seat.subscribe(id, order, hook)
                    .expect("fresh turn hook seat must accept core subscribers"),
            );
        }
        seat
    }

    /// Register a subscriber and return its explicit unsubscription handle.
    /// Subscriber ids are unique within one seat.
    pub fn subscribe(
        self: &Arc<Self>,
        id: &str,
        order: Order,
        hook: Arc<dyn TurnHook>,
    ) -> Result<Disposer, KernelError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(KernelError::Other(
                "turn-hooks subscriber id must be non-empty".into(),
            ));
        }
        let active = Arc::new(AtomicBool::new(true));
        {
            let mut entries = self.entries.write().expect("turn hook seat poisoned");
            if entries.iter().any(|entry| entry.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: String::new(),
                    service: format!("{TURN_HOOK_SEAT_SERVICE}:{id}"),
                });
            }
            entries.push(HookEntry {
                id: id.to_string(),
                order,
                active: active.clone(),
                hook,
            });
            entries.sort_by(|left, right| {
                left.order
                    .cmp(&right.order)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }

        let weak = Arc::downgrade(self);
        let id_for_dispose = id.to_string();
        let registered = active.clone();
        Ok(Disposer::new(move || {
            active.store(false, Ordering::Release);
            if let Some(seat) = weak.upgrade() {
                seat.entries
                    .write()
                    .expect("turn hook seat poisoned")
                    .retain(|entry| {
                        entry.id != id_for_dispose || !Arc::ptr_eq(&entry.active, &registered)
                    });
            }
        }))
    }

    /// Register a subscriber as an effect of `ctx`.
    pub fn subscribe_scoped(
        self: &Arc<Self>,
        ctx: &Context,
        id: &str,
        order: Order,
        hook: Arc<dyn TurnHook>,
    ) -> Result<(), KernelError> {
        let disposer = self.subscribe(id, order, hook)?;
        ctx.effect_labeled(&format!("turn hook({id})"), || disposer);
        Ok(())
    }

    pub fn subscriber_ids(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("turn hook seat poisoned")
            .iter()
            .filter(|entry| entry.active.load(Ordering::Acquire))
            .map(|entry| entry.id.clone())
            .collect()
    }

    pub(crate) fn emit_cache_trace(&self, event: &CacheTraceEvent<'_>) {
        TurnHookRuntime::from_snapshots(self.snapshot()).queue_cache_trace(event);
    }

    pub(crate) fn snapshot(&self) -> Vec<HookEntry> {
        self.entries
            .read()
            .expect("turn hook seat poisoned")
            .clone()
    }

    #[cfg(test)]
    fn dispatch_for_test(&self, event: &QueryEvent) -> TurnHookContext {
        TurnHookRuntime::from_snapshots(self.snapshot()).dispatch_event(event)
    }
}

/// Query-local hook sources.
#[derive(Clone)]
pub struct TurnHooks {
    seat: Option<Arc<TurnHookSeat>>,
    local: Vec<HookEntry>,
}

impl Default for TurnHooks {
    fn default() -> Self {
        Self {
            seat: Some(TurnHookSeat::new()),
            local: Vec::new(),
        }
    }
}

impl TurnHooks {
    pub fn with_seat(mut self, seat: Arc<TurnHookSeat>) -> Self {
        self.seat = Some(seat);
        self
    }

    pub fn with_hook(
        mut self,
        id: impl Into<String>,
        order: Order,
        hook: Arc<dyn TurnHook>,
    ) -> Self {
        let id = id.into();
        assert!(!id.trim().is_empty(), "turn hook id must be non-empty");
        self.local.push(HookEntry {
            id,
            order,
            active: Arc::new(AtomicBool::new(true)),
            hook,
        });
        self
    }

    pub fn has_seat(&self) -> bool {
        self.seat.is_some()
    }

    pub(crate) fn with_observer(self, observer: QueryEventObserver) -> Self {
        self.with_hook("core/query-event-observer", Order::LAST, Arc::new(observer))
    }

    #[cfg(test)]
    pub(crate) fn without_seat_for_test() -> Self {
        Self {
            seat: None,
            local: Vec::new(),
        }
    }
}

impl TurnHook for QueryEventObserver {
    fn on_event(&self, event: &QueryEvent, _context: &mut TurnHookContext) {
        self.call(event);
    }
}

/// The ordered snapshot used by one query turn.
///
/// `states` is index-aligned with `hooks`: one private slot per subscriber,
/// locked only for the length of a single callback so a subscriber that panics
/// mid-update cannot take the phase down with it.
pub(crate) struct TurnHookRuntime {
    hooks: Vec<HookEntry>,
    states: Vec<Mutex<TurnHookState>>,
    pending: Mutex<TurnHookContext>,
}

impl TurnHookRuntime {
    pub(crate) fn new(hooks: &TurnHooks) -> Arc<Self> {
        let mut snapshots = hooks
            .seat
            .as_ref()
            .map(|seat| seat.snapshot())
            .unwrap_or_default();
        snapshots.extend(hooks.local.clone());
        snapshots.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.id.cmp(&right.id))
        });
        Arc::new(Self::from_snapshots(snapshots))
    }

    pub(crate) fn from_snapshots(snapshots: Vec<HookEntry>) -> Self {
        let states = snapshots
            .iter()
            .map(|_| Mutex::new(TurnHookState::default()))
            .collect();
        Self {
            hooks: snapshots,
            states,
            pending: Mutex::new(TurnHookContext::default()),
        }
    }

    /// Start one controller drive. Every subscriber's per-drive state goes back to
    /// its initial value, which is what a context reset used to get for free by
    /// re-entering the loop with fresh locals.
    pub(crate) fn begin_drive(&self) {
        for state in &self.states {
            *state.lock().unwrap_or_else(PoisonError::into_inner) = TurnHookState::default();
        }
    }

    fn dispatch_event(&self, event: &QueryEvent) -> TurnHookContext {
        self.dispatch(|hook, context, _state| hook.on_event(event, context))
    }

    pub(crate) fn dispatch_attachment_poll(
        &self,
        event: &AttachmentPollHookEvent<'_>,
    ) -> TurnHookContext {
        self.dispatch(|hook, context, _state| hook.on_attachment_poll(event, context))
    }

    pub(crate) fn dispatch_tool_result(&self, event: &ToolResultHookEvent<'_>) {
        self.dispatch(|hook, _context, state| hook.on_tool_result(event, state));
    }

    pub(crate) fn dispatch_turn_end(&self, event: &TurnEndHookEvent<'_>) {
        let writeback = self.dispatch_until_continue(|hook, context, state| {
            hook.on_turn_end(event, context, state)
        });
        self.queue_writeback(writeback);
    }

    fn dispatch(
        &self,
        call: impl Fn(&dyn TurnHook, &mut TurnHookContext, &mut TurnHookState),
    ) -> TurnHookContext {
        self.dispatch_inner(false, call)
    }

    fn dispatch_until_continue(
        &self,
        call: impl Fn(&dyn TurnHook, &mut TurnHookContext, &mut TurnHookState),
    ) -> TurnHookContext {
        self.dispatch_inner(true, call)
    }

    fn dispatch_inner(
        &self,
        stop_on_continue: bool,
        call: impl Fn(&dyn TurnHook, &mut TurnHookContext, &mut TurnHookState),
    ) -> TurnHookContext {
        let mut combined = TurnHookContext::default();
        for (entry, state) in self.hooks.iter().zip(&self.states) {
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            let mut local = TurnHookContext::default();
            let result = catch_unwind(AssertUnwindSafe(|| {
                call(
                    entry.hook.as_ref(),
                    &mut local,
                    &mut state.lock().unwrap_or_else(PoisonError::into_inner),
                )
            }));
            match result {
                Ok(()) => {
                    let stop = stop_on_continue && local.continue_followups.is_some();
                    combined.append(local);
                    if stop {
                        break;
                    }
                }
                Err(_) => tracing::error!(
                    subscriber = %entry.id,
                    "turn hook panicked; writebacks discarded"
                ),
            }
        }
        combined
    }

    pub(crate) fn queue_event(&self, event: &QueryEvent) {
        let writeback = self.dispatch_event(event);
        self.queue_writeback(writeback);
    }

    /// Run the annotation phase over `output`, merging each subscriber's
    /// fields in subscriber order. Non-object outputs are left alone: there
    /// is nothing to add a field to.
    pub(crate) fn annotate_tool_result(
        &self,
        event: &ToolAnnotationHookEvent<'_>,
        output: &mut serde_json::Value,
    ) {
        let Some(object) = output.as_object_mut() else {
            return;
        };
        for entry in &self.hooks {
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            let mut context = ToolAnnotationHookContext::default();
            let result = catch_unwind(AssertUnwindSafe(|| {
                entry.hook.annotate_tool_result(event, &mut context);
            }));
            match result {
                Ok(()) => object.extend(context.fields),
                Err(_) => tracing::error!(
                    subscriber = %entry.id,
                    "turn hook panicked; tool-result annotations discarded"
                ),
            }
        }
    }

    pub(crate) async fn dispatch_tool_round(
        &self,
        event: &ToolRoundHookEvent<'_>,
    ) -> TurnHookContext {
        let mut combined = TurnHookContext::default();
        for entry in &self.hooks {
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            let task = catch_unwind(AssertUnwindSafe(|| entry.hook.on_tool_round(event)));
            let task = match task {
                Ok(task) => task,
                Err(_) => {
                    tracing::error!(
                        subscriber = %entry.id,
                        "turn hook panicked; tool-round work discarded"
                    );
                    continue;
                }
            };
            let Some(task) = task else {
                continue;
            };
            match AssertUnwindSafe(task).catch_unwind().await {
                Ok(context) => combined.append(context),
                Err(_) => tracing::error!(
                    subscriber = %entry.id,
                    "turn hook async work panicked; tool-round writebacks discarded"
                ),
            }
        }
        combined
    }

    pub(crate) fn dispatch_turn_budget(&self, event: &TurnBudgetEvent) -> TurnHookContext {
        self.dispatch(|hook, context, _state| hook.on_turn_budget(event, context))
    }

    pub(crate) fn queue_cache_trace(&self, event: &CacheTraceEvent<'_>) {
        if !rebon_api::cache_trace_enabled() {
            return;
        }
        let writeback = self.dispatch(|hook, _context, _state| hook.on_cache_trace(event));
        self.queue_writeback(writeback);
    }

    fn queue_writeback(&self, writeback: TurnHookContext) {
        self.pending
            .lock()
            .expect("turn hook writebacks poisoned")
            .append(writeback);
    }

    pub(crate) fn take_writeback(&self) -> TurnHookContext {
        std::mem::take(&mut *self.pending.lock().expect("turn hook writebacks poisoned"))
    }
}

#[derive(Clone)]
pub(crate) struct QueryEventSender {
    sender: tokio::sync::mpsc::UnboundedSender<QueryEvent>,
    hooks: Arc<TurnHookRuntime>,
    send_order: Arc<Mutex<()>>,
}

impl QueryEventSender {
    pub(crate) fn new(
        sender: tokio::sync::mpsc::UnboundedSender<QueryEvent>,
        hooks: &TurnHooks,
    ) -> Self {
        Self {
            sender,
            hooks: TurnHookRuntime::new(hooks),
            send_order: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn without_hooks(sender: tokio::sync::mpsc::UnboundedSender<QueryEvent>) -> Self {
        Self::new(sender, &TurnHooks::default())
    }

    fn ordered<T>(&self, call: impl FnOnce(&TurnHookRuntime) -> T) -> T {
        let _guard = self
            .send_order
            .lock()
            .expect("turn hook event ordering poisoned");
        call(&self.hooks)
    }

    pub(crate) fn send(
        &self,
        event: QueryEvent,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<QueryEvent>> {
        // Keep concurrent tool completions identical in hook and channel order.
        self.ordered(|hooks| {
            if !self.sender.is_closed() {
                hooks.queue_event(&event);
            }
            self.sender.send(event)
        })
    }

    /// Reset per-drive subscriber state at the top of one controller drive.
    pub(crate) fn begin_drive(&self) {
        self.hooks.begin_drive();
    }

    pub(crate) fn dispatch_turn_end(
        &self,
        message: &AssistantMessage,
        iteration: usize,
        history: &[ApiMessage],
        task_list_id: &str,
        params: &QueryParams,
    ) {
        self.ordered(|hooks| {
            hooks.dispatch_turn_end(&TurnEndHookEvent {
                message,
                iteration,
                max_iterations: params.max_iterations,
                history,
                task_list_id,
                attachment_poller: params.attachment_poller.as_ref(),
            })
        });
    }

    /// The annotation phase for one completed tool call. Ordered with the
    /// event channel like every other phase, so a concurrent completion
    /// cannot interleave a partly-annotated result.
    pub(crate) fn annotate_tool_result(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        cwd: Option<&str>,
        output: &mut serde_json::Value,
    ) {
        self.ordered(|hooks| {
            hooks.annotate_tool_result(
                &ToolAnnotationHookEvent {
                    tool_name,
                    input,
                    cwd,
                },
                output,
            )
        });
    }

    pub(crate) fn dispatch_tool_result(
        &self,
        tool_name: &str,
        output: Option<&serde_json::Value>,
        params: &QueryParams,
    ) {
        self.ordered(|hooks| {
            hooks.dispatch_tool_result(&ToolResultHookEvent {
                tool_name,
                output,
                attachment_poller: params.attachment_poller.as_ref(),
            })
        });
    }

    pub(crate) fn dispatch_attachment_poll(
        &self,
        phase: crate::query::AttachmentPollPhase,
        next_iteration: u64,
        history: &[ApiMessage],
        attachment_poller: Option<&crate::query::AttachmentPollerBinding>,
    ) -> TurnHookContext {
        self.ordered(|hooks| {
            hooks.dispatch_attachment_poll(&AttachmentPollHookEvent {
                phase,
                next_iteration,
                history,
                attachment_poller,
            })
        })
    }

    pub(crate) async fn dispatch_tool_round(
        &self,
        message: &AssistantMessage,
        iteration: usize,
        params: &QueryParams,
    ) -> TurnHookContext {
        self.hooks
            .dispatch_tool_round(&ToolRoundHookEvent {
                message,
                iteration,
                attachment_poller: params.attachment_poller.as_ref(),
                extensions: &params.extensions,
            })
            .await
    }

    pub(crate) fn dispatch_turn_budget(
        &self,
        iteration: usize,
        max_iterations: usize,
    ) -> TurnHookContext {
        self.ordered(|hooks| {
            hooks.dispatch_turn_budget(&TurnBudgetEvent {
                iteration,
                max_iterations,
            })
        })
    }

    pub(crate) fn emit_cache_trace(&self, event: &CacheTraceEvent<'_>) {
        self.ordered(|hooks| hooks.queue_cache_trace(event));
    }

    pub(crate) fn take_writeback(&self) -> TurnHookContext {
        self.hooks.take_writeback()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{QueryEvent, QueryParams};
    use std::sync::{Arc, Mutex};

    fn recording_hook(
        tag: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
    ) -> Arc<dyn TurnHook> {
        Arc::new(move |_event: &QueryEvent, _context: &mut TurnHookContext| {
            calls.lock().expect("turn hook calls poisoned").push(tag);
        })
    }

    /// The two writeback facts most phase tests assert on: what joined history
    /// and whether the phase asked the turn to continue.
    fn applied(writeback: TurnHookContext, params: &mut QueryParams) -> (Vec<ApiMessage>, bool) {
        let applied = writeback.apply_to_params(params);
        (applied.history, applied.continue_followups.is_some())
    }

    fn expected_turn_budget_warning(max_iterations: usize) -> ApiMessage {
        ApiMessage::user_text(format!(
            "[SYSTEM: You are approaching the iteration limit. \
             You have 10 iterations remaining out of {}. \
             Please wrap up your current work and provide a final response.]",
            max_iterations
        ))
    }

    #[test]
    fn subscribers_run_by_order_then_id() {
        let seat = TurnHookSeat::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let _late = seat
            .subscribe(
                "late",
                Order::new(20),
                recording_hook("late", calls.clone()),
            )
            .unwrap();
        let _same_b = seat
            .subscribe(
                "same-b",
                Order::NORMAL,
                recording_hook("same-b", calls.clone()),
            )
            .unwrap();
        let _early = seat
            .subscribe(
                "early",
                Order::new(-20),
                recording_hook("early", calls.clone()),
            )
            .unwrap();
        let _same_a = seat
            .subscribe(
                "same-a",
                Order::NORMAL,
                recording_hook("same-a", calls.clone()),
            )
            .unwrap();

        seat.dispatch_for_test(&QueryEvent::Cancelled);

        assert_eq!(
            *calls.lock().expect("turn hook calls poisoned"),
            ["early", "same-a", "same-b", "late"]
        );
    }

    #[test]
    fn disposer_unsubscribes_only_its_registration() {
        let seat = TurnHookSeat::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let gone = seat
            .subscribe("gone", Order::NORMAL, recording_hook("gone", calls.clone()))
            .unwrap();
        let _stays = seat
            .subscribe(
                "stays",
                Order::NORMAL,
                recording_hook("stays", calls.clone()),
            )
            .unwrap();

        seat.dispatch_for_test(&QueryEvent::Cancelled);
        gone.dispose();
        seat.dispatch_for_test(&QueryEvent::Cancelled);

        assert_eq!(
            *calls.lock().expect("turn hook calls poisoned"),
            ["gone", "stays", "stays"]
        );
    }

    #[test]
    fn scoped_subscription_stops_an_in_flight_snapshot_when_context_disposes() {
        let kernel = rebon_kernel::Kernel::new();
        let context = kernel.context().fork("plugin");
        let seat = TurnHookSeat::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        seat.subscribe_scoped(
            &context,
            "scoped",
            Order::NORMAL,
            recording_hook("scoped", calls.clone()),
        )
        .unwrap();
        let runtime = TurnHookRuntime::from_snapshots(seat.snapshot());

        context.dispose();
        runtime.dispatch_event(&QueryEvent::Cancelled);

        assert!(calls.lock().expect("turn hook calls poisoned").is_empty());
        assert_eq!(
            seat.subscriber_ids(),
            [
                ATTACHMENT_INJECTION_HOOK_ID.to_string(),
                TERMINAL_ATTACHMENT_HOOK_ID.to_string(),
                TASK_TURN_RECONCILIATION_HOOK_ID.to_string(),
                CACHE_TRACE_HOOK_ID.to_string(),
                TURN_BUDGET_WARNING_HOOK_ID.to_string(),
            ]
        );
    }

    #[test]
    fn panicking_subscriber_does_not_block_later_hooks_or_commit_partial_writes() {
        let seat = TurnHookSeat::new();
        let _panics = seat
            .subscribe(
                "a-panics",
                Order::NORMAL,
                Arc::new(|_event: &QueryEvent, context: &mut TurnHookContext| {
                    context.append_history(ApiMessage::user_text("discard me"));
                    panic!("subscriber failed");
                }),
            )
            .unwrap();
        let _survives = seat
            .subscribe(
                "b-survives",
                Order::NORMAL,
                Arc::new(|_event: &QueryEvent, context: &mut TurnHookContext| {
                    context.append_history(ApiMessage::user_text("keep me"));
                }),
            )
            .unwrap();

        let writeback = seat.dispatch_for_test(&QueryEvent::Cancelled);
        let mut params = QueryParams::new("test", Vec::new());
        let (history, _) = applied(writeback, &mut params);

        assert_eq!(history, [ApiMessage::user_text("keep me")]);
    }

    #[test]
    fn raw_event_observer_uses_the_same_event_pipeline() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observer = QueryEventObserver::new({
            let calls = calls.clone();
            move |_event| {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        });
        let hooks = TurnHooks::default().with_observer(observer);
        let runtime = TurnHookRuntime::new(&hooks);

        runtime.queue_event(&QueryEvent::Cancelled);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn subscriber_writeback_appends_history_and_updates_optional_params() {
        let seat = TurnHookSeat::new();
        let _subscription = seat
            .subscribe(
                "writer",
                Order::NORMAL,
                Arc::new(|_event: &QueryEvent, context: &mut TurnHookContext| {
                    context.append_history(rebon_api::Message::user_text("hook history"));
                    context.update_params(|params| {
                        params.transient_context_message = Some("hook params".into());
                    });
                    context.request_continue();
                }),
            )
            .unwrap();

        let writeback = seat.dispatch_for_test(&QueryEvent::Cancelled);
        let mut params = QueryParams::new("test", Vec::new());
        let (history, continue_turn) = applied(writeback, &mut params);

        assert_eq!(history, [rebon_api::Message::user_text("hook history")]);
        assert_eq!(
            params.transient_context_message.as_deref(),
            Some("hook params")
        );
        assert!(continue_turn);
    }

    #[tokio::test]
    async fn tool_round_phase_honors_order_disposal_and_sync_and_async_panic_isolation() {
        struct ToolRoundPhaseHook {
            tag: &'static str,
            calls: Arc<Mutex<Vec<&'static str>>>,
            panic_synchronously: bool,
            panic_asynchronously: bool,
        }

        impl TurnHook for ToolRoundPhaseHook {
            fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

            fn on_tool_round(&self, _event: &ToolRoundHookEvent<'_>) -> Option<TurnHookFuture> {
                if self.panic_synchronously {
                    self.calls
                        .lock()
                        .expect("tool-round calls poisoned")
                        .push(self.tag);
                    panic!("synchronous tool-round panic");
                }
                let tag = self.tag;
                let calls = self.calls.clone();
                let panic_asynchronously = self.panic_asynchronously;
                Some(Box::pin(async move {
                    calls.lock().expect("tool-round calls poisoned").push(tag);
                    if panic_asynchronously {
                        panic!("asynchronous tool-round panic");
                    }
                    let mut context = TurnHookContext::default();
                    context.append_history(ApiMessage::user_text(tag));
                    context
                }))
            }
        }

        let seat = TurnHookSeat::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let disposed = seat
            .subscribe(
                "tests/a-disposed-tool-round",
                Order::FIRST,
                Arc::new(ToolRoundPhaseHook {
                    tag: "disposed",
                    calls: calls.clone(),
                    panic_synchronously: false,
                    panic_asynchronously: false,
                }),
            )
            .unwrap();
        let _sync_panic = seat
            .subscribe(
                "tests/b-sync-panic-tool-round",
                Order::FIRST,
                Arc::new(ToolRoundPhaseHook {
                    tag: "sync-panic",
                    calls: calls.clone(),
                    panic_synchronously: true,
                    panic_asynchronously: false,
                }),
            )
            .unwrap();
        let _async_panic = seat
            .subscribe(
                "tests/c-async-panic-tool-round",
                Order::FIRST,
                Arc::new(ToolRoundPhaseHook {
                    tag: "async-panic",
                    calls: calls.clone(),
                    panic_synchronously: false,
                    panic_asynchronously: true,
                }),
            )
            .unwrap();
        let _survivor = seat
            .subscribe(
                "tests/d-surviving-tool-round",
                Order::FIRST,
                Arc::new(ToolRoundPhaseHook {
                    tag: "survivor",
                    calls: calls.clone(),
                    panic_synchronously: false,
                    panic_asynchronously: false,
                }),
            )
            .unwrap();
        let runtime = TurnHookRuntime::from_snapshots(seat.snapshot());
        disposed.dispose();
        let message = AssistantMessage {
            id: "msg-tool-round".into(),
            model: "mock".into(),
            content: vec![rebon_api::ContentBlock::ToolUse(rebon_api::ToolUseBlock {
                id: "toolu-round".into(),
                name: "Other".into(),
                input: serde_json::json!({}),
            })],
            stop_reason: Some(rebon_api::StopReason::ToolUse),
            usage: Usage::default(),
        };
        let mut params = QueryParams::new("test", Vec::new());

        let writeback = runtime
            .dispatch_tool_round(&ToolRoundHookEvent::completed(&message, 0, &params))
            .await;
        let (history, continue_turn) = applied(writeback, &mut params);

        assert_eq!(
            *calls.lock().expect("tool-round calls poisoned"),
            ["sync-panic", "async-panic", "survivor"]
        );
        assert_eq!(history, [ApiMessage::user_text("survivor")]);
        assert!(!continue_turn);
    }

    // ── terminal waterfall ────────────────────────────────────────

    /// A poller that answers one recorded batch and records what it was asked.
    #[derive(Default)]
    struct RecordingPoller {
        injected: Vec<ApiMessage>,
        report_paths: Mutex<Vec<std::path::PathBuf>>,
        polls: Mutex<Vec<(u64, crate::query::AttachmentPollPhase)>>,
        plan_mode_calls: Mutex<Vec<(String, bool)>>,
        task_tool_calls: Mutex<Vec<u64>>,
    }

    impl crate::query::AttachmentPoller for RecordingPoller {
        fn poll(&self, request: crate::query::AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
            self.polls
                .lock()
                .expect("poll log poisoned")
                .push((request.next_iteration, request.phase));
            self.injected.clone()
        }

        fn take_coordinator_report_paths_for_query(
            &self,
            _session_id: &str,
            _turn_id: &str,
        ) -> Vec<std::path::PathBuf> {
            std::mem::take(&mut *self.report_paths.lock().expect("report paths poisoned"))
        }

        fn notify_plan_mode_tool(
            &self,
            tool_name: &str,
            succeeded: bool,
            _tool_result: Option<&serde_json::Value>,
        ) {
            self.plan_mode_calls
                .lock()
                .expect("plan mode log poisoned")
                .push((tool_name.to_string(), succeeded));
        }

        fn notify_task_tool_used(&self, iteration: u64) {
            self.task_tool_calls
                .lock()
                .expect("task tool log poisoned")
                .push(iteration);
        }
    }

    fn params_with_poller(poller: Arc<RecordingPoller>, max_iterations: usize) -> QueryParams {
        let mut params = QueryParams::new("test", Vec::new()).with_max_iterations(max_iterations);
        params.attachment_poller = Some(crate::query::AttachmentPollerBinding::new(
            poller, "session", "turn",
        ));
        params
    }

    fn terminal_message(stop_reason: Option<rebon_api::StopReason>) -> AssistantMessage {
        AssistantMessage {
            id: "msg_terminal".into(),
            model: "mock".into(),
            content: vec![rebon_api::ContentBlock::Text(rebon_api::TextBlock {
                text: "final".into(),
            })],
            stop_reason,
            usage: Usage::default(),
        }
    }

    /// Run the terminal phase once and return the applied writeback.
    fn drive_turn_end(
        runtime: &TurnHookRuntime,
        params: &mut QueryParams,
        iteration: usize,
        history: &[ApiMessage],
        message: AssistantMessage,
    ) -> TurnHookContext {
        runtime.dispatch_turn_end(&TurnEndHookEvent::terminal_candidate(
            &message,
            iteration,
            history,
            "tests/no-task-list",
            params,
        ));
        runtime.take_writeback().apply_to_params(params)
    }

    #[test]
    fn terminal_eager_attachment_continues_the_turn_and_stops_the_waterfall() {
        let poller = Arc::new(RecordingPoller {
            injected: vec![ApiMessage::user_text("fresh attachment")],
            report_paths: Mutex::new(vec![std::path::PathBuf::from("/reports/a.md")]),
            ..RecordingPoller::default()
        });
        let seat = TurnHookSeat::new();
        let later_ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _later = seat
            .subscribe("zz-later", Order::LAST, {
                let later_ran = later_ran.clone();
                Arc::new(CountingTurnEndHook { calls: later_ran })
            })
            .unwrap();
        let runtime = TurnHookRuntime::from_snapshots(seat.snapshot());
        let mut params = params_with_poller(poller.clone(), 5);

        let applied = drive_turn_end(&runtime, &mut params, 1, &[], terminal_message(None));

        assert_eq!(
            applied.continue_followups,
            Some(TERMINAL_ATTACHMENT_FOLLOWUP_ITERATIONS)
        );
        // The assistant response precedes the attachment it is answered against.
        assert_eq!(applied.history.len(), 1);
        assert_eq!(applied.history[0].role, rebon_api::Role::Assistant);
        assert_eq!(
            applied.injected_attachments,
            [ApiMessage::user_text("fresh attachment")]
        );
        assert_eq!(
            applied.coordinator_report_paths,
            [std::path::PathBuf::from("/reports/a.md")]
        );
        assert_eq!(
            *poller.polls.lock().expect("poll log poisoned"),
            [(2, crate::query::AttachmentPollPhase::Eager)]
        );
        assert_eq!(later_ran.load(Ordering::SeqCst), 0);
    }

    struct CountingTurnEndHook {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TurnHook for CountingTurnEndHook {
        fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

        fn on_turn_end(
            &self,
            _event: &TurnEndHookEvent<'_>,
            _context: &mut TurnHookContext,
            _state: &mut TurnHookState,
        ) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn terminal_eager_attachment_declines_at_the_cap_and_on_repeated_history() {
        let listing = format!(
            "{}\n\n- commit: write a commit message",
            crate::attachments::SKILL_LISTING_INITIAL_HEADER
        );
        let poller = Arc::new(RecordingPoller {
            injected: vec![ApiMessage::user_text(listing.clone())],
            ..RecordingPoller::default()
        });
        let runtime = TurnHookRuntime::from_snapshots(TurnHookSeat::new().snapshot());
        let mut params = params_with_poller(poller.clone(), 2);

        // At the cap the phase does not even poll: the turn has no base
        // iteration left for an answer.
        let at_cap = drive_turn_end(&runtime, &mut params, 2, &[], terminal_message(None));
        assert!(at_cap.continue_followups.is_none());
        assert!(poller.polls.lock().expect("poll log poisoned").is_empty());

        // Below the cap it polls, but an attachment the replayed history
        // already carries verbatim is neither announced nor answered.
        let history = [ApiMessage::user_text(listing)];
        let repeated = drive_turn_end(&runtime, &mut params, 0, &history, terminal_message(None));
        assert_eq!(poller.polls.lock().expect("poll log poisoned").len(), 1);
        assert!(repeated.continue_followups.is_none());
        assert!(repeated.injected_attachments.is_empty());
        assert!(repeated.history.is_empty());
    }

    #[test]
    fn scheduled_attachment_polls_announce_without_continuing_the_turn() {
        let poller = Arc::new(RecordingPoller {
            injected: vec![ApiMessage::user_text("regular attachment")],
            ..RecordingPoller::default()
        });
        let runtime = TurnHookRuntime::from_snapshots(TurnHookSeat::new().snapshot());
        let mut params = params_with_poller(poller.clone(), 5);

        let applied = runtime
            .dispatch_attachment_poll(&AttachmentPollHookEvent::scheduled(
                crate::query::AttachmentPollPhase::Regular,
                3,
                &[],
                &params,
            ))
            .apply_to_params(&mut params);

        assert_eq!(
            *poller.polls.lock().expect("poll log poisoned"),
            [(3, crate::query::AttachmentPollPhase::Regular)]
        );
        assert_eq!(
            applied.injected_attachments,
            [ApiMessage::user_text("regular attachment")]
        );
        assert!(applied.history.is_empty());
        assert!(applied.continue_followups.is_none());
    }

    #[test]
    fn tool_results_reach_the_poller_in_order_with_their_success() {
        let poller = Arc::new(RecordingPoller::default());
        let runtime = TurnHookRuntime::from_snapshots(TurnHookSeat::new().snapshot());
        let params = params_with_poller(poller.clone(), 5);
        let output = serde_json::json!({"ok": true});

        for (name, result) in [
            ("ExitPlanMode", Some(&output)),
            ("EnterPlanMode", None),
            ("Read", Some(&output)),
        ] {
            runtime.dispatch_tool_result(&ToolResultHookEvent::completed(name, result, &params));
        }

        assert_eq!(
            *poller
                .plan_mode_calls
                .lock()
                .expect("plan mode log poisoned"),
            [
                ("ExitPlanMode".to_string(), true),
                ("EnterPlanMode".to_string(), false),
                ("Read".to_string(), true),
            ]
        );
    }

    /// A subscriber adds fields; the runtime merges them into the output in
    /// subscriber order and never lets one rewrite what the tool returned.
    #[test]
    fn annotations_are_merged_into_a_successful_tool_result() {
        struct Annotator(&'static str, &'static str);
        impl TurnHook for Annotator {
            fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}
            fn annotate_tool_result(
                &self,
                event: &ToolAnnotationHookEvent<'_>,
                context: &mut ToolAnnotationHookContext,
            ) {
                if event.tool_name != "Write" {
                    return;
                }
                let path = event.input.get("file_path").cloned().unwrap_or_default();
                context.annotate(self.0, serde_json::json!(self.1));
                context.annotate("saw", serde_json::json!([path, event.cwd]));
            }
        }
        let seat = TurnHookSeat::new();
        drop(
            seat.subscribe("b", Order::NORMAL, Arc::new(Annotator("second", "b")))
                .unwrap(),
        );
        drop(
            seat.subscribe("a", Order::FIRST, Arc::new(Annotator("first", "a")))
                .unwrap(),
        );
        let runtime = TurnHookRuntime::from_snapshots(seat.snapshot());

        let input = serde_json::json!({"file_path": "/repo/MEMORY.md"});
        let mut output = serde_json::json!({"type": "create"});
        runtime.annotate_tool_result(
            &ToolAnnotationHookEvent {
                tool_name: "Write",
                input: &input,
                cwd: Some("/repo"),
            },
            &mut output,
        );
        assert_eq!(output["type"], "create", "the tool's own fields survive");
        assert_eq!(output["first"], "a");
        assert_eq!(output["second"], "b");
        assert_eq!(
            output["saw"],
            serde_json::json!(["/repo/MEMORY.md", "/repo"])
        );

        // A subscriber that declines leaves the output untouched.
        let mut other = serde_json::json!({"type": "read"});
        runtime.annotate_tool_result(
            &ToolAnnotationHookEvent {
                tool_name: "Read",
                input: &input,
                cwd: Some("/repo"),
            },
            &mut other,
        );
        assert_eq!(other, serde_json::json!({"type": "read"}));
    }

    /// A panicking subscriber loses its own fields and no one else's, and a
    /// non-object output is left alone rather than replaced.
    #[test]
    fn a_panicking_annotator_is_isolated() {
        struct Boom;
        impl TurnHook for Boom {
            fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}
            fn annotate_tool_result(
                &self,
                _event: &ToolAnnotationHookEvent<'_>,
                context: &mut ToolAnnotationHookContext,
            ) {
                context.annotate("discarded", serde_json::json!(true));
                panic!("subscriber blew up");
            }
        }
        struct Good;
        impl TurnHook for Good {
            fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}
            fn annotate_tool_result(
                &self,
                _event: &ToolAnnotationHookEvent<'_>,
                context: &mut ToolAnnotationHookContext,
            ) {
                context.annotate("kept", serde_json::json!(true));
            }
        }
        let seat = TurnHookSeat::new();
        drop(
            seat.subscribe("boom", Order::FIRST, Arc::new(Boom))
                .unwrap(),
        );
        drop(seat.subscribe("good", Order::LAST, Arc::new(Good)).unwrap());
        let runtime = TurnHookRuntime::from_snapshots(seat.snapshot());

        let input = serde_json::json!({});
        let event = ToolAnnotationHookEvent {
            tool_name: "Write",
            input: &input,
            cwd: None,
        };
        let mut output = serde_json::json!({});
        runtime.annotate_tool_result(&event, &mut output);
        assert_eq!(output, serde_json::json!({"kept": true}));

        let mut scalar = serde_json::json!("a string result");
        runtime.annotate_tool_result(&event, &mut scalar);
        assert_eq!(scalar, serde_json::json!("a string result"));
    }

    #[tokio::test]
    async fn a_round_notifies_the_task_throttle_only_when_a_task_tool_ran() {
        let poller = Arc::new(RecordingPoller::default());
        let runtime = TurnHookRuntime::from_snapshots(TurnHookSeat::new().snapshot());
        let params = params_with_poller(poller.clone(), 5);

        let without = tool_round_message("Read");
        runtime
            .dispatch_tool_round(&ToolRoundHookEvent::completed(&without, 4, &params))
            .await;
        assert!(poller
            .task_tool_calls
            .lock()
            .expect("task tool log poisoned")
            .is_empty());

        let with = tool_round_message("TaskUpdate");
        runtime
            .dispatch_tool_round(&ToolRoundHookEvent::completed(&with, 4, &params))
            .await;
        assert_eq!(
            *poller
                .task_tool_calls
                .lock()
                .expect("task tool log poisoned"),
            [4]
        );
    }

    fn tool_round_message(tool_name: &str) -> AssistantMessage {
        AssistantMessage {
            id: "msg_round".into(),
            model: "mock".into(),
            content: vec![rebon_api::ContentBlock::ToolUse(rebon_api::ToolUseBlock {
                id: "toolu_1".into(),
                name: tool_name.into(),
                input: serde_json::json!({}),
            })],
            stop_reason: Some(rebon_api::StopReason::ToolUse),
            usage: Usage::default(),
        }
    }

    #[test]
    fn a_new_drive_restarts_per_subscriber_state() {
        struct CountingStateHook;

        #[derive(Default)]
        struct Counter(usize);

        impl TurnHook for CountingStateHook {
            fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

            fn on_tool_result(&self, _event: &ToolResultHookEvent<'_>, state: &mut TurnHookState) {
                state.get_mut::<Counter>().0 += 1;
            }

            fn on_turn_end(
                &self,
                _event: &TurnEndHookEvent<'_>,
                context: &mut TurnHookContext,
                state: &mut TurnHookState,
            ) {
                context.append_history(ApiMessage::user_text(format!(
                    "{}",
                    state.get_mut::<Counter>().0
                )));
            }
        }

        let seat = TurnHookSeat::new();
        let _counting = seat
            .subscribe(
                "tests/counting-state",
                Order::FIRST,
                Arc::new(CountingStateHook),
            )
            .unwrap();
        let runtime = TurnHookRuntime::from_snapshots(seat.snapshot());
        let mut params = QueryParams::new("test", Vec::new());
        let output = serde_json::json!({});
        for _ in 0..3 {
            runtime.dispatch_tool_result(&ToolResultHookEvent::completed(
                "Read",
                Some(&output),
                &params,
            ));
        }

        let first = drive_turn_end(&runtime, &mut params, 0, &[], terminal_message(None));
        runtime.begin_drive();
        let second = drive_turn_end(&runtime, &mut params, 0, &[], terminal_message(None));

        assert_eq!(first.history, [ApiMessage::user_text("3")]);
        assert_eq!(second.history, [ApiMessage::user_text("0")]);
    }

    #[test]
    fn turn_budget_warning_preserves_threshold_off_by_one_and_message_contract() {
        let runtime = TurnHookRuntime::from_snapshots(TurnHookSeat::new().snapshot());

        let mut params = QueryParams::new("test", Vec::new());
        let mut history = Vec::new();
        for event in [
            TurnBudgetEvent::after_tool_round(0, 12),
            TurnBudgetEvent::after_tool_round(0, 10),
        ] {
            let (mut event_history, continue_turn) =
                applied(runtime.dispatch_turn_budget(&event), &mut params);
            assert!(!continue_turn);
            history.append(&mut event_history);
        }
        let threshold = TurnBudgetEvent::after_tool_round(1, 12);
        assert_eq!(threshold.iteration, 1);
        assert_eq!(threshold.max_iterations, 12);
        assert_eq!(threshold.remaining_iterations(), 10);
        let (mut threshold_history, continue_turn) =
            applied(runtime.dispatch_turn_budget(&threshold), &mut params);
        assert!(!continue_turn);
        history.append(&mut threshold_history);
        let (mut post_threshold_history, continue_turn) = applied(
            runtime.dispatch_turn_budget(&TurnBudgetEvent::after_tool_round(2, 12)),
            &mut params,
        );
        assert!(!continue_turn);
        history.append(&mut post_threshold_history);

        assert_eq!(history, [expected_turn_budget_warning(12)]);
    }

    #[test]
    fn turn_budget_phase_honors_disposal_and_isolates_panics() {
        struct BudgetPhaseHook {
            calls: Arc<std::sync::atomic::AtomicUsize>,
            message: Option<&'static str>,
            panics: bool,
        }

        impl TurnHook for BudgetPhaseHook {
            fn on_event(&self, _event: &QueryEvent, _context: &mut TurnHookContext) {}

            fn on_turn_budget(&self, _event: &TurnBudgetEvent, context: &mut TurnHookContext) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(message) = self.message {
                    context.append_history(ApiMessage::user_text(message));
                }
                if self.panics {
                    panic!("budget hook panic");
                }
            }
        }

        let seat = TurnHookSeat::new();
        let disposed_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let disposed = seat
            .subscribe(
                "tests/a-disposed-budget-hook",
                Order::FIRST,
                Arc::new(BudgetPhaseHook {
                    calls: disposed_calls.clone(),
                    message: None,
                    panics: false,
                }),
            )
            .unwrap();
        let panic_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _panics = seat
            .subscribe(
                "tests/b-panicking-budget-hook",
                Order::FIRST,
                Arc::new(BudgetPhaseHook {
                    calls: panic_calls.clone(),
                    message: Some("discard me"),
                    panics: true,
                }),
            )
            .unwrap();
        let survivor_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _survives = seat
            .subscribe(
                "tests/c-surviving-budget-hook",
                Order::FIRST,
                Arc::new(BudgetPhaseHook {
                    calls: survivor_calls.clone(),
                    message: Some("keep me"),
                    panics: false,
                }),
            )
            .unwrap();
        let runtime = TurnHookRuntime::from_snapshots(seat.snapshot());
        disposed.dispose();

        let mut params = QueryParams::new("test", Vec::new());
        let (history, _) = applied(
            runtime.dispatch_turn_budget(&TurnBudgetEvent::after_tool_round(0, 11)),
            &mut params,
        );

        assert_eq!(disposed_calls.load(Ordering::SeqCst), 0);
        assert_eq!(panic_calls.load(Ordering::SeqCst), 1);
        assert_eq!(survivor_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            history,
            [
                ApiMessage::user_text("keep me"),
                expected_turn_budget_warning(11),
            ]
        );
    }

    #[test]
    fn migrated_crosscuts_have_one_turn_control_owner() {
        let query_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/query");
        assert!(
            !query_dir.join("run_loop.rs").exists(),
            "the retired query loop must not coexist with turn_control"
        );

        let executor_path = query_dir.join("executor.rs");
        let executor = std::fs::read_to_string(&executor_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", executor_path.display()));
        for marker in [
            "rebon_cache_trace",
            "cache_trace_enabled()",
            "[SYSTEM: You are approaching the iteration limit.",
            "remaining == 10",
            "SkillState::on_files_touched",
            "extract_file_paths",
            "Progressive skill discovery",
        ] {
            assert!(
                !executor.contains(marker),
                "{} retains migrated inline marker {marker:?}",
                executor_path.display()
            );
        }

        let controller_path = query_dir.join("turn_control.rs");
        let controller = std::fs::read_to_string(&controller_path).unwrap_or_else(|error| {
            panic!("failed to read {}: {error}", controller_path.display())
        });
        assert_eq!(controller.matches("dispatch_turn_budget(").count(), 1);
        assert_eq!(controller.matches("dispatch_tool_round(").count(), 1);
        assert_eq!(controller.matches("dispatch_turn_end(").count(), 1);
        assert_eq!(controller.matches("dispatch_attachment_poll(").count(), 2);
        assert_eq!(controller.matches("dispatch_tool_result(").count(), 1);
        assert_eq!(controller.matches("emit_cache_trace(").count(), 2);
    }
}
