//! Running the tool calls of one model response as ordered stages.
//!
//! A model that plans a sequence often sends it as one batch — write a file,
//! then test it; make a directory, then `cd` into it. Running every call of
//! the batch at once races those steps, so the batch runs in stages instead:
//!
//! * a maximal run of consecutive concurrency-safe calls is one stage, and
//!   its calls run in parallel;
//! * every other call is a barrier: a stage of its own, which waits for
//!   everything before it and holds back everything after it.
//!
//! Whether a call is concurrency-safe is the tool's own answer
//! ([`rebon_tool::Tool::is_concurrency_safe`]), with two exceptions the tool
//! cannot answer alone: `Agent` also depends on how many writers the batch
//! has, and `InvokeDeferredTool` is whatever the tool it names is.
//!
//! Once a barrier fails — an error, or a shell command that exited non-zero
//! or ran out of time — the barriers after it in the same batch are not run:
//! they were planned on top of a step that did not happen. Each comes back as
//! an error naming the failed call, so the model re-issues them after the
//! fix. Concurrency-safe calls only read, so they still run.

use super::*;
use crate::turn_hook::QueryEventSender;

/// One tool call of the batch, with everything it needs to run decided
/// before the first stage starts.
pub(super) struct PreparedToolCall {
    /// Position in the response; results are restored to this order.
    pub(super) index: usize,
    pub(super) id: String,
    pub(super) name: String,
    pub(super) input: Value,
    pub(super) context: ToolContext,
    /// The call's input was cut off by the output token limit.
    pub(super) truncated_input: Option<String>,
    /// The tool is not exposed in this session.
    pub(super) filter_reject: bool,
    /// May share a stage with its concurrency-safe neighbours.
    pub(super) concurrent: bool,
}

/// A finished call: index, id, tool name and outcome.
pub(super) type IndexedOutcome = (usize, String, String, Result<Value, ToolErrorPresentation>);

/// Whether the call `name(input)` may run in parallel with its
/// concurrency-safe neighbours.
///
/// An unknown or unresolvable tool is treated as a barrier: nothing says
/// what it would have done, and failing closed only costs parallelism.
pub(super) fn call_is_concurrency_safe(
    tools: &dyn rebon_tool::ToolResolver,
    context: &ToolContext,
    name: &str,
    input: &Value,
    parallel_agent_write_batch: bool,
) -> bool {
    if name == rebon_tool::AGENT_TOOL_NAME {
        return rebon_tool::agent_call_is_concurrency_safe(input, parallel_agent_write_batch);
    }
    if name == rebon_tool::INVOKE_DEFERRED_TOOL_NAME {
        let Some(target) = input.get("tool_name").and_then(Value::as_str) else {
            return false;
        };
        if target == rebon_tool::INVOKE_DEFERRED_TOOL_NAME {
            return false;
        }
        let arguments = input.get("arguments").cloned().unwrap_or(Value::Null);
        return call_is_concurrency_safe(
            tools,
            context,
            target,
            &arguments,
            parallel_agent_write_batch,
        );
    }
    match tools.resolve(name, context.tool_filter()) {
        Ok(Some(tool)) => tool.is_concurrency_safe(input),
        _ => false,
    }
}

/// Split a batch into stages: each maximal run of concurrency-safe calls is
/// one stage, and each other call is a stage of its own.
pub(super) fn plan_stages(concurrent: &[bool]) -> Vec<std::ops::Range<usize>> {
    let mut stages = Vec::new();
    let mut start = 0;
    while start < concurrent.len() {
        let end = if concurrent[start] {
            start
                + concurrent[start..]
                    .iter()
                    .take_while(|concurrent| **concurrent)
                    .count()
        } else {
            start + 1
        };
        stages.push(start..end);
        start = end;
    }
    stages
}

/// The error a barrier gets when an earlier barrier of its batch failed.
/// `failed_index` is the failed call's 0-based position in the response.
pub(super) fn skipped_after_failure_message(failed_index: usize, failed_tool: &str) -> String {
    format!(
        "skipped: call #{} ({failed_tool}) failed earlier in this batch; re-issue after fixing it",
        failed_index + 1
    )
}

/// Whether a call's outcome means the step did not happen as planned.
///
/// A shell tool reports a failing command as a successful call with a
/// non-zero `exitCode` (or `timedOut`), and the step after `cargo build` is
/// as stranded by that as by a tool error. A `null` exit code is a
/// background shell still running, not a failure.
pub(super) fn outcome_failed(outcome: &Result<Value, ToolErrorPresentation>) -> bool {
    match outcome {
        Err(_) => true,
        Ok(value) => {
            value
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
                || value.get("timedOut").and_then(Value::as_bool) == Some(true)
        }
    }
}

fn interrupted() -> ToolErrorPresentation {
    ToolErrorPresentation::same("cancelled", "Interrupted by user")
}

fn send_result(
    tx: &QueryEventSender,
    id: &str,
    name: &str,
    outcome: &Result<Value, ToolErrorPresentation>,
) {
    let error_presentation = outcome
        .as_ref()
        .err()
        .filter(|error| error.has_distinct_display_message())
        .cloned();
    let _ = tx.send(QueryEvent::ToolDispatchResult {
        tool_use_id: id.to_string(),
        name: name.to_string(),
        outcome: outcome.clone().map_err(|error| error.model_message.clone()),
        error_presentation,
    });
}

fn send_start(tx: &QueryEventSender, call: &PreparedToolCall) {
    let _ = tx.send(QueryEvent::ToolDispatchStart {
        tool_use_id: call.id.clone(),
        name: call.name.clone(),
        input: call.input.clone(),
    });
}

/// Start one call now. `ToolDispatchStart` goes out here, when the call
/// really starts, not when the response arrived.
fn spawn_call(
    engine: &Arc<Engine>,
    call: PreparedToolCall,
    tx: &QueryEventSender,
    cancel: &CancelToken,
    policy: crate::policy_seat::PolicySources,
) -> tokio::task::JoinHandle<Result<Value, ToolErrorPresentation>> {
    send_start(tx, &call);
    let engine = Arc::clone(engine);
    let tx = tx.clone();
    let cancel = cancel.clone();
    tokio::spawn(async move {
        let PreparedToolCall {
            id,
            name,
            input,
            context,
            truncated_input,
            filter_reject,
            ..
        } = call;
        let result_tx = tx.clone();
        let result_id = id.clone();
        let result_name = name.clone();
        let dispatch = async move {
            if let Some(reason) = truncated_input {
                Err(ToolErrorPresentation::same("output_truncated", reason))
            } else if filter_reject {
                Err(ToolErrorPresentation::same(
                    "tool_unavailable",
                    format!("tool `{name}` is not available in this session"),
                ))
            } else {
                let tool_use = ToolUseBlock { id, name, input };
                super::turn_control::dispatch_tool_use(&engine, &tool_use, &context, &tx, policy)
                    .await
            }
        };
        tokio::pin!(dispatch);
        let outcome = tokio::select! {
            biased;
            _ = cancel.notified() => Err(interrupted()),
            outcome = &mut dispatch => outcome,
        };
        send_result(&result_tx, &result_id, &result_name, &outcome);
        outcome
    })
}

/// Run `calls` (in response order) stage by stage and return every outcome,
/// in response order.
///
/// A call still queued when the turn is cancelled never starts: it gets a
/// `ToolDispatchResult` of "Interrupted by user" and no `ToolDispatchStart`,
/// the same as a call cancelled before the batch began.
pub(super) async fn run_staged(
    engine: &Arc<Engine>,
    calls: Vec<PreparedToolCall>,
    tx: &QueryEventSender,
    cancel: &CancelToken,
    policy: &crate::policy_seat::PolicySources,
) -> Vec<IndexedOutcome> {
    let flags: Vec<bool> = calls.iter().map(|call| call.concurrent).collect();
    let mut results = Vec::with_capacity(calls.len());
    let mut first_failed_barrier: Option<(usize, String)> = None;
    let mut calls = calls.into_iter();

    for stage in plan_stages(&flags) {
        let stage_calls: Vec<PreparedToolCall> = calls.by_ref().take(stage.len()).collect();
        if cancel.is_cancelled() {
            for call in stage_calls {
                let outcome = Err(interrupted());
                send_result(tx, &call.id, &call.name, &outcome);
                results.push((call.index, call.id, call.name, outcome));
            }
            continue;
        }

        let mut running = Vec::with_capacity(stage_calls.len());
        for call in stage_calls {
            if let (false, Some((failed_index, failed_tool))) =
                (call.concurrent, first_failed_barrier.as_ref())
            {
                let outcome = Err(ToolErrorPresentation::same(
                    "skipped_after_failure",
                    skipped_after_failure_message(*failed_index, failed_tool),
                ));
                send_start(tx, &call);
                send_result(tx, &call.id, &call.name, &outcome);
                results.push((call.index, call.id, call.name, outcome));
                continue;
            }
            let meta = (
                call.index,
                call.id.clone(),
                call.name.clone(),
                call.concurrent,
            );
            running.push((meta, spawn_call(engine, call, tx, cancel, policy.clone())));
        }

        for ((index, id, name, concurrent), handle) in running {
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(join_err) => {
                    tracing::error!(error = %join_err, "tool dispatch task panicked");
                    let outcome = Err(ToolErrorPresentation::same(
                        "tool_panicked",
                        format!("tool `{name}` failed unexpectedly: {join_err}"),
                    ));
                    send_result(tx, &id, &name, &outcome);
                    outcome
                }
            };
            if !concurrent && outcome_failed(&outcome) && first_failed_barrier.is_none() {
                first_failed_barrier = Some((index, name.clone()));
            }
            results.push((index, id, name, outcome));
        }
    }

    results.sort_by_key(|(index, ..)| *index);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn consecutive_safe_calls_share_a_stage_and_barriers_stand_alone() {
        assert_eq!(
            plan_stages(&[true, true, false, true, false, false, true]),
            vec![0..2, 2..3, 3..4, 4..5, 5..6, 6..7]
        );
        assert_eq!(plan_stages(&[true, true, true]), vec![0..3]);
        assert_eq!(plan_stages(&[false, false]), vec![0..1, 1..2]);
        assert!(plan_stages(&[]).is_empty());
    }

    #[test]
    fn a_non_zero_exit_or_timeout_is_a_failure_but_a_running_shell_is_not() {
        assert!(outcome_failed(&Err(interrupted())));
        assert!(outcome_failed(&Ok(json!({"exitCode": 2}))));
        assert!(outcome_failed(&Ok(
            json!({"exitCode": 0, "timedOut": true})
        )));
        assert!(!outcome_failed(&Ok(
            json!({"exitCode": 0, "timedOut": false})
        )));
        assert!(!outcome_failed(&Ok(
            json!({"shellId": "s1", "exitCode": null})
        )));
        assert!(!outcome_failed(&Ok(json!({"type": "update"}))));
    }

    #[test]
    fn skip_message_names_the_failed_call_one_based() {
        assert_eq!(
            skipped_after_failure_message(1, "Bash"),
            "skipped: call #2 (Bash) failed earlier in this batch; re-issue after fixing it"
        );
    }
}
