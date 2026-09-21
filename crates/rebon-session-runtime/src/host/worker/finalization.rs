use super::super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackgroundExecutionOutcome {
    PromptCompleted,
    PromptAlreadyPersisted,
    PromptCancelled,
    /// A `UserPromptSubmit` hook refused the claimed prompt before a turn
    /// started: nothing ran, nothing was written, and the prompt is not
    /// retried — the hook would refuse it again.
    PromptRefused {
        reason: String,
    },
    IdleWarmEnded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundTurnFinalization {
    Applied,
    Queued,
    CancelledOrSuperseded,
    Stopped,
}

pub(crate) fn current_worker_owns_turn(
    current: &BackgroundJobState,
    turn_generation: u64,
    ipc: &BackgroundIpcServer,
) -> bool {
    let expected_owner = RecordedOwnerSnapshot::owned(
        std::process::id(),
        ipc.pid_identity.clone(),
        process_owns_detached_group(std::process::id()),
        false,
        Some(ipc.port),
        Some(ipc.token.clone()),
        turn_generation,
    );
    current.recorded_owner() == expected_owner
}

pub(crate) fn classify_turn_finalization(
    current: &BackgroundJobState,
    turn_generation: u64,
    ipc: &BackgroundIpcServer,
) -> BackgroundTurnFinalization {
    if current.process.status == BackgroundJobStatus::Stopped {
        BackgroundTurnFinalization::Stopped
    } else if current_worker_owns_turn(current, turn_generation, ipc)
        && matches!(
            current.process.status,
            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
        )
    {
        BackgroundTurnFinalization::Applied
    } else {
        BackgroundTurnFinalization::CancelledOrSuperseded
    }
}

pub(crate) fn claimed_pending_prompts_completion(
    current: &BackgroundJobState,
    claimed_state: &BackgroundJobState,
    transcript_completed: bool,
) -> anyhow::Result<Option<bool>> {
    let claimed = claimed_pending_prompts(claimed_state);
    if claimed.is_empty() {
        return Ok(None);
    }
    let Some(current_claimed) = current.identity.pending_prompts.get(..claimed.len()) else {
        anyhow::bail!("claimed pending prompts disappeared before finalization");
    };
    if current_claimed
        .iter()
        .zip(claimed)
        .any(|(current, claimed)| {
            current.id != claimed.id
                || current.claimed_turn_generation != Some(claimed_state.process.turn_generation)
        })
    {
        anyhow::bail!("claimed pending prompts no longer own the queue prefix");
    }
    Ok(Some(
        current_claimed
            .iter()
            .all(|prompt| prompt.completed_turn_generation.is_some())
            && transcript_completed,
    ))
}

pub(crate) fn retire_claimed_pending_prompts(
    current: &mut BackgroundJobState,
    claimed_state: &BackgroundJobState,
) -> anyhow::Result<()> {
    let claimed = claimed_pending_prompts(claimed_state);
    if claimed.is_empty() {
        return Ok(());
    }
    let Some(current_claimed) = current.identity.pending_prompts.get(..claimed.len()) else {
        anyhow::bail!("claimed pending prompts disappeared before finalization");
    };
    if current_claimed
        .iter()
        .zip(claimed)
        .any(|(current, claimed)| {
            current.id != claimed.id
                || current.claimed_turn_generation != Some(claimed_state.process.turn_generation)
        })
    {
        anyhow::bail!("claimed pending prompts no longer own the queue prefix");
    }
    current.identity.pending_prompts.drain(..claimed.len());
    Ok(())
}

pub(crate) fn mark_claimed_pending_prompts_completed(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
) -> anyhow::Result<bool> {
    let claimed_ids = claimed_pending_prompts(state)
        .iter()
        .map(|prompt| prompt.id.clone())
        .collect::<Vec<_>>();
    if claimed_ids.is_empty() {
        return Ok(true);
    }
    let completed_at = now_ms();
    store.update_state(&state.identity.job_id, |current| {
        if classify_turn_finalization(current, state.process.turn_generation, ipc)
            != BackgroundTurnFinalization::Applied
        {
            return Ok(false);
        }
        let Some(current_claimed) = current.identity.pending_prompts.get(..claimed_ids.len())
        else {
            anyhow::bail!("claimed pending prompts disappeared before completion ack");
        };
        if current_claimed
            .iter()
            .zip(&claimed_ids)
            .any(|(prompt, id)| {
                prompt.id != *id
                    || prompt.claimed_turn_generation != Some(state.process.turn_generation)
            })
        {
            anyhow::bail!("claimed pending prompts no longer own the queue prefix");
        }
        for prompt in &mut current.identity.pending_prompts[..claimed_ids.len()] {
            prompt.completed_turn_generation = Some(state.process.turn_generation);
        }
        current.process.updated_at_ms = completed_at;
        Ok(true)
    })
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BackgroundTurnTerminalOutcome<'a> {
    Cancelled,
    Completed {
        summary: &'a str,
    },
    Failed {
        message: &'a str,
        summary: &'a str,
    },
    /// A hook refused the prompt before the turn began. Recorded like a
    /// failure — the job shows why — but the claim is retired: unlike a
    /// turn that died, retrying would only be refused again.
    Refused {
        message: &'a str,
        summary: &'a str,
    },
}

impl BackgroundTurnTerminalOutcome<'_> {
    fn queued_summary(self) -> &'static str {
        match self {
            Self::Cancelled => "queued follow-up after cancelled turn",
            Self::Completed { .. } => "queued follow-up after completed turn",
            Self::Failed { .. } => "queued follow-up after failed turn",
            Self::Refused { .. } => "queued follow-up after refused prompt",
        }
    }
}

pub(crate) fn finalize_background_turn(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    outcome: BackgroundTurnTerminalOutcome<'_>,
    completed_at: u64,
) -> anyhow::Result<BackgroundTurnFinalization> {
    let transcript_completed = !matches!(outcome, BackgroundTurnTerminalOutcome::Cancelled)
        && pending_prompt_transcript_state(state) == PendingPromptTranscriptState::Completed;
    finalize_background_turn_with_completion_evidence(
        store,
        state,
        ipc,
        outcome,
        completed_at,
        transcript_completed,
    )
}

pub(crate) fn finalize_background_turn_with_completion_evidence(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    outcome: BackgroundTurnTerminalOutcome<'_>,
    completed_at: u64,
    transcript_completed: bool,
) -> anyhow::Result<BackgroundTurnFinalization> {
    let mut cancelled_permission_query_id = None;
    let finalization = store.update_state(&state.identity.job_id, |current| {
        let finalization = classify_turn_finalization(current, state.process.turn_generation, ipc);
        if finalization != BackgroundTurnFinalization::Applied {
            return Ok(finalization);
        }
        let claimed_completion =
            claimed_pending_prompts_completion(current, state, transcript_completed)?;
        let retire_claimed = match (outcome, claimed_completion) {
            (_, None) => false,
            (BackgroundTurnTerminalOutcome::Cancelled, Some(_)) => true,
            (BackgroundTurnTerminalOutcome::Completed { .. }, Some(true)) => true,
            (BackgroundTurnTerminalOutcome::Completed { .. }, Some(false)) => {
                anyhow::bail!(
                    "claimed pending prompt completed without a verified durable transcript response"
                );
            }
            (BackgroundTurnTerminalOutcome::Failed { .. }, Some(completed)) => completed,
            (BackgroundTurnTerminalOutcome::Refused { .. }, Some(_)) => true,
        };
        if retire_claimed {
            retire_claimed_pending_prompts(current, state)?;
        }
        cancelled_permission_query_id = current
            .outcome.pending_permission
            .as_ref()
            .map(|pending| pending.query_id);
        current.outcome.pending_permission = None;
        current.outcome.summary_updated_at_ms = None;
        current.process.updated_at_ms = completed_at;

        let retained_incomplete_claim = matches!(
            (outcome, claimed_completion),
            (BackgroundTurnTerminalOutcome::Failed { .. }, Some(false))
        );
        if current.has_pending_prompts() && !retained_incomplete_claim {
            current.process.status = BackgroundJobStatus::Queued;
            current.outcome.summary = Some(outcome.queued_summary().to_string());
            current.process.completed_at_ms = None;
            current.outcome.exit_code = None;
            current.outcome.error = None;
            return Ok(BackgroundTurnFinalization::Queued);
        }

        match outcome {
            BackgroundTurnTerminalOutcome::Cancelled => {
                current.process.status = BackgroundJobStatus::Idle;
                current.outcome.summary = Some("turn cancelled".to_string());
                current.process.completed_at_ms = None;
                current.outcome.exit_code = None;
                current.outcome.error = None;
            }
            BackgroundTurnTerminalOutcome::Completed { summary } => {
                current.process.status = BackgroundJobStatus::Succeeded;
                current.outcome.summary = Some(summary.to_string());
                current.process.completed_at_ms = Some(completed_at);
                current.outcome.exit_code = Some(0);
                current.outcome.error = None;
            }
            BackgroundTurnTerminalOutcome::Failed { message, summary }
            | BackgroundTurnTerminalOutcome::Refused { message, summary } => {
                current.process.status = BackgroundJobStatus::Failed;
                current.outcome.summary = Some(summary.to_string());
                current.process.completed_at_ms = Some(completed_at);
                current.outcome.exit_code = Some(1);
                current.outcome.error = Some(message.to_string());
            }
        }
        Ok(BackgroundTurnFinalization::Applied)
    })?;
    // The record now carries the turn's outcome and the usage it added.
    // Published after the write rather than with the `Turn Idle` event,
    // which goes out before it: a snapshot taken then would still show the
    // turn running and the total without this turn in it.
    if matches!(
        finalization,
        BackgroundTurnFinalization::Applied | BackgroundTurnFinalization::Queued
    ) {
        ipc.publish_status_now(store, &state.identity.job_id);
    }
    if cancelled_permission_query_id
        .is_some_and(|query_id| ipc.cancel_permission_response(query_id))
    {
        let _ = store.append_event(
            &state.identity.job_id,
            "permissions_cancelled_ipc",
            serde_json::json!({ "count": 1 }),
        );
    }
    Ok(finalization)
}

pub(crate) fn finalize_cancelled_background_turn(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    cancelled_at: u64,
) -> anyhow::Result<BackgroundTurnFinalization> {
    finalize_background_turn(
        store,
        state,
        ipc,
        BackgroundTurnTerminalOutcome::Cancelled,
        cancelled_at,
    )
}

pub(crate) fn finalize_completed_background_turn(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    summary: &str,
    completed_at: u64,
) -> anyhow::Result<BackgroundTurnFinalization> {
    finalize_background_turn(
        store,
        state,
        ipc,
        BackgroundTurnTerminalOutcome::Completed { summary },
        completed_at,
    )
}

pub(crate) fn finalize_failed_background_turn(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    message: &str,
    summary: &str,
    completed_at: u64,
) -> anyhow::Result<BackgroundTurnFinalization> {
    finalize_background_turn(
        store,
        state,
        ipc,
        BackgroundTurnTerminalOutcome::Failed { message, summary },
        completed_at,
    )
}

pub(crate) fn finalize_refused_background_turn(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    message: &str,
    summary: &str,
    completed_at: u64,
) -> anyhow::Result<BackgroundTurnFinalization> {
    finalize_background_turn(
        store,
        state,
        ipc,
        BackgroundTurnTerminalOutcome::Refused { message, summary },
        completed_at,
    )
}
