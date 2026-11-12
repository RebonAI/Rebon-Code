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

/// Why a barrier's outcome strands the barriers after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stranded {
    /// The step failed.
    Failed,
    /// A shell command outlived its timeout and continues in the background:
    /// the step has not finished, so what comes after it cannot rely on it.
    StillRunning,
}

/// The error a barrier gets when an earlier barrier of its batch stranded
/// it. `index` is that call's 0-based position in the response.
pub(super) fn skipped_message(index: usize, tool: &str, why: Stranded) -> String {
    let call = index + 1;
    match why {
        Stranded::Failed => format!(
            "skipped: call #{call} ({tool}) failed earlier in this batch; re-issue after fixing it"
        ),
        Stranded::StillRunning => format!(
            "skipped: call #{call} ({tool}) is still running in the background after its \
             timeout; re-issue once it has finished"
        ),
    }
}

/// Whether a call's outcome means the step did not happen as planned.
///
/// Any tool error does. Beyond that, only a call that ran a command in this
/// call — `Bash` or `PowerShell` — is read for how the command went: other
/// tools carry an `exitCode` that describes something else (`ShellStop`
/// reporting a shell that finished long ago). For those, a non-zero
/// `exitCode`, a `timedOut`, or a terminating `signal` (a segfault, an
/// out-of-memory kill) leaves the step after `cargo build` as stranded as a
/// tool error would. So does a command moved to the background at its
/// timeout (`backgrounded`): it has not finished. A `null` exit code alone is
/// a `run_in_background` launch, which the model asked for, not a failure.
pub(super) fn stranding(
    tool_name: &str,
    outcome: &Result<Value, ToolErrorPresentation>,
) -> Option<Stranded> {
    let value = match outcome {
        Err(_) => return Some(Stranded::Failed),
        Ok(value) => value,
    };
    if !runs_a_command(tool_name) {
        return None;
    }
    if value.get("backgrounded").and_then(Value::as_bool) == Some(true) {
        return Some(Stranded::StillRunning);
    }
    let failed = value
        .get("exitCode")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
        || value.get("timedOut").and_then(Value::as_bool) == Some(true)
        || value.get("signal").is_some_and(|signal| !signal.is_null());
    failed.then_some(Stranded::Failed)
}

/// The tools whose result reports a command they ran in this call, under any
/// name the model may use for them.
fn runs_a_command(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Bash" | "BashTool" | "bash" | "PowerShell" | "PowerShellTool"
    )
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
    let mut first_stranding_barrier: Option<(usize, String, Stranded)> = None;
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
            if let (false, Some((stranding_index, stranding_tool, why))) =
                (call.concurrent, first_stranding_barrier.as_ref())
            {
                let outcome = Err(ToolErrorPresentation::same(
                    "skipped_after_failure",
                    skipped_message(*stranding_index, stranding_tool, *why),
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
            if !concurrent && first_stranding_barrier.is_none() {
                if let Some(why) = stranding(&name, &outcome) {
                    first_stranding_barrier = Some((index, name.clone(), why));
                }
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
    fn a_non_zero_exit_or_timeout_is_a_failure_but_a_background_launch_is_not() {
        assert_eq!(
            stranding("Bash", &Err(interrupted())),
            Some(Stranded::Failed)
        );
        assert_eq!(
            stranding("Edit", &Err(interrupted())),
            Some(Stranded::Failed)
        );
        for name in ["Bash", "bash", "BashTool", "PowerShell"] {
            assert_eq!(
                stranding(name, &Ok(json!({"exitCode": 2}))),
                Some(Stranded::Failed),
                "{name}"
            );
        }
        assert_eq!(
            stranding("Bash", &Ok(json!({"exitCode": 0, "timedOut": true}))),
            Some(Stranded::Failed)
        );
        assert_eq!(
            stranding("Bash", &Ok(json!({"exitCode": 0, "timedOut": false}))),
            None
        );
        assert_eq!(
            stranding("Bash", &Ok(json!({"shellId": "s1", "exitCode": null}))),
            None
        );
        assert_eq!(stranding("Edit", &Ok(json!({"type": "update"}))), None);
    }

    /// `ShellStop` on a shell that finished long ago succeeds and echoes
    /// that shell's exit code; it ran no command, so it strands nothing.
    #[test]
    fn an_exit_code_from_a_tool_that_ran_no_command_is_not_a_failure() {
        let stop_noop = json!({
            "shellId": "sh_1",
            "status": "exited",
            "alreadyCompleted": true,
            "exitCode": 2,
        });
        assert_eq!(stranding("ShellStop", &Ok(stop_noop)), None);
        assert_eq!(
            stranding("TaskStop", &Ok(json!({"exitCode": 1, "timedOut": true}))),
            None
        );
    }

    /// A command killed by a signal (a segfault, an out-of-memory kill)
    /// reports no exit code; it failed all the same.
    #[test]
    fn a_command_killed_by_a_signal_is_a_failure() {
        assert_eq!(
            stranding(
                "Bash",
                &Ok(json!({"exitCode": null, "timedOut": false, "signal": 11}))
            ),
            Some(Stranded::Failed)
        );
        assert_eq!(
            stranding("Bash", &Ok(json!({"exitCode": null, "signal": null}))),
            None
        );
    }

    /// A foreground command moved to the background at its timeout has not
    /// finished, which strands what comes after it.
    #[test]
    fn a_command_backgrounded_at_its_timeout_strands_the_rest() {
        assert_eq!(
            stranding(
                "Bash",
                &Ok(json!({
                    "exitCode": null,
                    "timedOut": true,
                    "backgrounded": true,
                    "shellId": "sh_1"
                }))
            ),
            Some(Stranded::StillRunning)
        );
    }

    #[test]
    fn skip_message_names_the_call_one_based_and_says_why() {
        assert_eq!(
            skipped_message(1, "Bash", Stranded::Failed),
            "skipped: call #2 (Bash) failed earlier in this batch; re-issue after fixing it"
        );
        assert_eq!(
            skipped_message(0, "Bash", Stranded::StillRunning),
            "skipped: call #1 (Bash) is still running in the background after its timeout; \
             re-issue once it has finished"
        );
    }
}
