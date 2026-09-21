use super::super::*;
use rebon_core::hooks::{
    append_additional_context, apply_user_prompt_submit_effects, UserPromptSubmitDecision,
};
use rebon_permissions::PermissionMode;
use rebon_session_host::BackgroundRetryProgress;

use crate::title::{completed_session_title_for, persist_session_title, session_title_update};

/// Put the session on the model the record names, before the turn starts.
///
/// `/model` on a mirrored terminal writes the job record; this is where the
/// worker picks it up, the same place and the same way it picks up the effort
/// level. Reading it once at session build was the reason `/model` reported
/// success and then changed nothing — the answer even said so, in the smallest
/// print available ("it applies when the session is next built"), and hosted is
/// every session by default. A client told a change landed has to be able to
/// believe it.
///
/// A model that will not resolve does not take the turn down with it. The
/// session keeps the client it already has, the failure is written to the job's
/// event log where the Agent View shows it, and the prompt still runs — on the
/// old model, which is the one the session was working. A name typed wrong is
/// something to report, not a session to lose.
async fn apply_recorded_model(
    session: &mut crate::EngineSession,
    state: &BackgroundJobState,
    store: &BackgroundStore,
) {
    let Some(wanted) = state
        .identity
        .runtime
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return;
    };
    if wanted == session.model.name {
        return;
    }
    let previous = session.model.name.clone();
    // Resolved through the same path a local `/model` takes, with the record's
    // model named as the override: the job record is where a hosted session's
    // model lives, and reading the global config here would answer with
    // whatever this machine last typed into some other terminal.
    let overrides = crate::rebon_config::RuntimeOverride {
        model: Some(wanted.to_string()),
        ..Default::default()
    };
    match crate::runtime_refresh::resolve_and_install_runtime(session, overrides).await {
        Ok(()) => {
            tracing::info!(
                job_id = %state.identity.job_id,
                from = %previous,
                to = %session.model.name,
                "rebon: worker switched model for this turn"
            );
            let _ = store.append_event(
                &state.identity.job_id,
                "session_model_switched",
                serde_json::json!({ "from": previous, "to": session.model.name }),
            );
        }
        Err(err) => {
            tracing::warn!(
                job_id = %state.identity.job_id,
                wanted,
                error = %err,
                "rebon: worker kept its model; the recorded one would not resolve"
            );
            let _ = store.append_event(
                &state.identity.job_id,
                "session_model_switch_failed",
                serde_json::json!({
                    "wanted": wanted,
                    "keeping": previous,
                    "error": err.to_string(),
                }),
            );
        }
    }
}

/// How often the turn checks whether the model client is retrying.
///
/// Slow enough to cost nothing on a turn that never retries, quick enough that
/// a client watching the job sees the first retry rather than the tail of it.
const RETRY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Mirrors the model client's retry state onto the job, on change only.
///
/// The state file is read by every client watching this job, so writing it each
/// tick would be a write every 500ms for the length of the turn. Retries are
/// rare and their state barely moves, so a change check turns that into a
/// handful of writes across a whole rate-limited turn — and none at all on a
/// turn that never retries.
fn publish_retry_progress(
    store: &BackgroundStore,
    job_id: &str,
    notifier: &rebon_api::RetryNotifier,
    published: &mut Option<BackgroundRetryProgress>,
) {
    let current = notifier.current().map(|progress| BackgroundRetryProgress {
        attempt: progress.attempt,
        max_retries: progress.max_retries,
    });
    if current == *published {
        return;
    }
    // Best-effort: a turn is not worth failing because its progress indicator
    // could not be written.
    if let Err(err) = store.update_state(job_id, |state| {
        state.outcome.retry = current;
        Ok(())
    }) {
        tracing::debug!(%job_id, error = %err, "could not publish retry progress");
        return;
    }
    *published = current;
}

/// The skill a typed `/name …` names, when this session has it and a user
/// may invoke it.
///
/// A worker's prompts arrive as the text a client typed, and until this the
/// worker asked for no skill invocations at all — so `/review` reached the
/// model as a line of prose and the skill never loaded. A terminal hosting
/// its own session resolves the same line against the same registry before
/// the turn starts (`submit.rs`); doing it here gives every client of a
/// worker — the mirror terminal, the desktop window, a browser tab — the
/// behaviour the terminal always had.
///
/// A name the registry does not know is left alone: it may be a slash
/// command the turn loop handles, or just a line that starts with a slash.
fn user_skill_invocations(
    session: &crate::EngineSession,
    prompt: &str,
) -> Vec<rebon_agent_core::SkillInvocationRequest> {
    let Some((skill, args)) = rebon_plugin_skill::parse_user_skill_invocation(prompt) else {
        return Vec::new();
    };
    let registry = &session.engine_half.skill_registry;
    if registry.is_disabled(&skill) {
        tracing::info!(%skill, "rebon worker: skipping a disabled skill named in a prompt");
        return Vec::new();
    }
    let Some(definition) = registry.get(&skill) else {
        return Vec::new();
    };
    if !definition.user_invocable {
        tracing::info!(%skill, "rebon worker: this skill is the model's to invoke, not the user's");
        return Vec::new();
    }
    vec![rebon_agent_core::SkillInvocationRequest { skill, args }]
}

/// Publish `<sid>.owner.json` so a client holding only a session id can find
/// this worker's endpoint without scanning `jobs/`.
///
/// A gate, not a courtesy (design §4.2, §12.2 item 1). This used to be best
/// effort: a failed write was logged and the turn ran anyway, which produced
/// the one state RFC-0004 §I3 forbids — a session being written by a host
/// nobody can reach. A client would see the transcript growing, find no
/// descriptor, and have no way to send to the process doing the writing; the
/// only exit was to stop the job.
///
/// So it fails the build instead, and it is called before the first write:
/// the caller drops the session (releasing the lock), the endpoint is never
/// published, and the job lands in the same terminal finalizer as any other
/// start-up failure, with `Failed` and a message saying what could not be
/// written. Losing the session is the smaller harm, and it is a harm the user
/// is told about rather than one they have to diagnose.
///
/// The file is removed with the lock, so a crash cannot leave one that reads
/// as live.
///
/// Takes the three fields it needs rather than the whole session: what it
/// publishes is a location, and a caller assembling one should not have to
/// have built a session first — which is also what lets this be tested.
pub(crate) fn publish_session_owner_descriptor(
    projects_root: &std::path::Path,
    cwd: &str,
    session_id: &str,
    job_id: &str,
    ipc: &BackgroundIpcServer,
) -> anyhow::Result<()> {
    let descriptor = rebon_session::SessionOwnerDescriptor {
        version: rebon_session::SESSION_OWNER_VERSION,
        pid: std::process::id(),
        pid_identity: ipc.pid_identity.clone(),
        surface: rebon_session::SessionOwnerSurface::Worker,
        job_id: Some(job_id.to_string()),
        ipc_port: Some(ipc.port),
        ipc_token: Some(ipc.token.clone()),
        started_at_ms: now_ms(),
    };
    rebon_session::write_session_owner(projects_root, cwd, session_id, &descriptor).map_err(|err| {
        anyhow::anyhow!(
            "could not publish the session owner descriptor for {session_id}, \
                 so no client could reach this worker: {err}"
        )
    })
}

/// Hand the escalation gate the acceptances a person already made.
///
/// The door this room has is answered by a person: the desktop app hosts every
/// chat as a background job, and the plan dialog it shows is theirs. Their
/// choice used to be dropped — `ExitPlanMode` reported success, the session
/// stayed in `plan`, and the model looped on the reminder forever. A job
/// already refuses to *launch* in these modes without an interactive
/// acceptance, so that acceptance is exactly the authorization the gate is
/// looking for; nothing here grants a mode the launch door would have refused.
fn declare_preauthorized_escalations() {
    for mode in [PermissionMode::Auto, PermissionMode::BypassPermissions] {
        if crate::rebon_config::background_permission_mode_is_accepted(mode) {
            rebon_tool::set_escalation_preauthorized(mode.as_wire());
        }
    }
}

pub async fn run_background_worker(job_id: String) -> anyhow::Result<()> {
    // A background job is a room with a door but nobody in it. Its
    // permission prompts and `AskUserQuestion` calls queue on the IPC
    // server and are answered whenever someone opens the job, so the
    // tools that ask stay exposed — unlike `rebon exec`, which declares
    // `Unattended` and loses them. What the job cannot have is a
    // permission mode it granted itself: `ExitPlanMode` handing back
    // `auto` mid-run walks straight past
    // `ensure_background_permission_mode_allowed`, which refuses those
    // modes at launch without a prior interactive acceptance. Declaring
    // the surface is what closes that door.
    rebon_tool::set_execution_surface(rebon_tool::ExecutionSurface::Detached);
    declare_preauthorized_escalations();

    let store = cli_default_store();
    let ipc = start_background_ipc_server(&store, &job_id)?;
    // Held across turns, not per session: see `release_session_keeping_lock`.
    // The lock dropping when this function returns is what publishes the
    // session as free — and what removes `<sid>.owner.json` beside it. The
    // MCP stack is kept for the worker's life for the same reason and because
    // the servers are worth keeping, and torn down before the lock goes so the
    // session never reads as free while its servers are still up. The task
    // registry is kept so a task created in one turn is still resolvable in
    // the next.
    let mut handed_back = crate::session_handoff::HandedBack::default();
    let result = run_worker_turns(&store, &job_id, &ipc, &mut handed_back).await;
    // The one line that says a worker chose to leave, as opposed to being
    // killed: a worker that vanishes with nothing here was taken down from
    // outside.
    tracing::info!(
        %job_id,
        outcome = ?result.as_ref().map(|_| "done").map_err(|err| err.to_string()),
        "rebon: background worker exiting"
    );
    close_host_resources(&ipc, handed_back.mcp.take(), &store, &job_id).await;
    result
}

/// Close what this host holds, in the order a client can observe.
///
/// Two steps, and the order between them is the contract rather than an
/// implementation detail:
///
/// 1. Release every call still waiting on this host, before the resources they
///    are waiting for go away (design §5.3 item 6). The same path a single
///    `CancelCall` takes, so a client parked on a `/compact` gets the same
///    answer whether it gave up or the worker did.
/// 2. Then close the MCP transports, bounded.
///
/// Backwards, a waiter would be woken by a transport that had already gone and
/// would see a torn-down stack instead of a typed refusal. It is a named
/// function so that order is a thing a test can watch happen, which is what
/// design §12.2 item 9 asks for; the wider claim those make
/// — every host closed before the process plugin plane — is structural, because
/// every host lives inside `route_main` and the plane is closed after it
/// returns.
pub(crate) async fn close_host_resources(
    ipc: &BackgroundIpcServer,
    mcp: Option<crate::mcp::SessionMcp>,
    store: &BackgroundStore,
    job_id: &str,
) {
    let released = ipc.cancel_calls_in_flight();
    if released > 0 {
        tracing::info!(
            %job_id,
            released,
            "rebon: released calls still waiting when the worker exited"
        );
    }
    if let Some(mcp) = mcp {
        // Bounded: a worker that decided to exit must exit. The transport
        // shutdown awaits servers hanging up, and a peer that never does —
        // an lsp-mcp daemon with its own lifetime, a wedged plugin host —
        // held the "exiting" worker alive for the better part of an hour.
        //
        // The deadline is the transport's to keep, not this call site's.
        // Wrapping the unbounded shutdown in a timeout here used to drop its
        // future mid-teardown, which is what left servers running: whatever
        // the loop had not reached kept its child process. Asking the
        // transport to close within a budget means every server is cancelled
        // — waiters released, children killed — and only the join can be cut
        // short.
        let report = mcp.close_within(MCP_SHUTDOWN_BUDGET).await;
        if !report.is_complete() {
            // Recorded, not only logged: a log line dies with the process, and
            // "these MCP servers were still closing when the worker left" is
            // exactly the fact a person debugging a stuck lsp-mcp daemon
            // needs. Not folded into the job's result, because the turn itself
            // was fine — only the teardown ran long.
            tracing::warn!(
                %job_id,
                budget_ms = MCP_SHUTDOWN_BUDGET.as_millis() as u64,
                servers = %report.timed_out.join(", "),
                "rebon: MCP servers were still closing when the budget ran out"
            );
            let _ = store.append_event(
                &job_id,
                "mcp_shutdown_deadline_exceeded",
                serde_json::json!({
                    "budgetMs": MCP_SHUTDOWN_BUDGET.as_millis() as u64,
                    "servers": report.timed_out,
                }),
            );
        }
    }
}

/// How long a worker waits for its MCP servers to hang up before leaving.
///
/// Named so the deadline and the event that reports it cannot drift apart.
const MCP_SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

async fn run_worker_turns(
    store: &BackgroundStore,
    job_id: &str,
    ipc: &BackgroundIpcServer,
    handed_back: &mut crate::session_handoff::HandedBack,
) -> anyhow::Result<()> {
    loop {
        // Publish the token and claim the persisted turn while holding the same
        // lock used by Reply/Cancel, so those operations cannot cancel a token
        // installed for a later state generation.
        let (turn_cancel, claim) = ipc.start_turn_and(|| {
            claim_background_job(
                &store,
                &job_id,
                std::process::id(),
                ipc.port,
                &ipc.token,
                process_is_running,
            )
        });
        let mut state = match claim? {
            WorkerClaim::Claimed(state) => *state,
            WorkerClaim::NoWork => {
                if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                    continue;
                }
                return Ok(());
            }
            WorkerClaim::Stopped => {
                store.append_event(&job_id, "worker_skipped_stopped", serde_json::json!({}))?;
                return Ok(());
            }
            WorkerClaim::OwnedByOther(owner) => {
                store.append_event(
                    &job_id,
                    "worker_yielded_duplicate",
                    serde_json::json!({ "pid": std::process::id(), "ownerPid": owner }),
                )?;
                return Ok(());
            }
        };
        store.append_event(
            &job_id,
            "worker_started",
            serde_json::json!({ "pid": state.process.pid }),
        )?;

        let result = execute_background_job(store, &mut state, ipc, turn_cancel, handed_back).await;
        match result {
            Ok(BackgroundExecutionOutcome::IdleWarmEnded) => return Ok(()),
            Ok(BackgroundExecutionOutcome::PromptCancelled) => {
                let cancelled_at = now_ms();
                let finalization =
                    finalize_cancelled_background_turn(&store, &state, &ipc, cancelled_at)?;
                match finalization {
                    BackgroundTurnFinalization::Queued => continue,
                    BackgroundTurnFinalization::Stopped => return Ok(()),
                    BackgroundTurnFinalization::Applied
                    | BackgroundTurnFinalization::CancelledOrSuperseded => {}
                }
                store.append_event(&job_id, "turn_cancelled", serde_json::json!({}))?;
                if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                    continue;
                }
                return Ok(());
            }
            Ok(BackgroundExecutionOutcome::PromptRefused { reason }) => {
                let summary = background_job_failure_summary(&reason);
                let completed_at = now_ms();
                let finalization = finalize_refused_background_turn(
                    &store,
                    &state,
                    &ipc,
                    &reason,
                    &summary,
                    completed_at,
                )?;
                match finalization {
                    BackgroundTurnFinalization::Queued => continue,
                    BackgroundTurnFinalization::Stopped => return Ok(()),
                    BackgroundTurnFinalization::CancelledOrSuperseded => {
                        if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                            continue;
                        }
                        return Ok(());
                    }
                    BackgroundTurnFinalization::Applied => {}
                }
                store.append_event(
                    &job_id,
                    "prompt_refused",
                    serde_json::json!({ "error": reason }),
                )?;
                // The prompt was refused; the worker is fine. It waits for
                // the next one like any worker whose turn just ended.
                if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                    continue;
                }
                return Ok(());
            }
            Ok(
                BackgroundExecutionOutcome::PromptCompleted
                | BackgroundExecutionOutcome::PromptAlreadyPersisted,
            ) => {
                let summary = background_job_success_summary_from_store(&store, &job_id)
                    .unwrap_or_else(|| background_job_success_summary(&state));
                let completed_at = now_ms();
                let finalization = finalize_completed_background_turn(
                    &store,
                    &state,
                    &ipc,
                    &summary,
                    completed_at,
                )?;
                match finalization {
                    BackgroundTurnFinalization::Queued => continue,
                    BackgroundTurnFinalization::Stopped => return Ok(()),
                    BackgroundTurnFinalization::CancelledOrSuperseded => {
                        if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                            continue;
                        }
                        return Ok(());
                    }
                    BackgroundTurnFinalization::Applied => {}
                }
                store.append_event(&job_id, "completed", serde_json::json!({}))?;
                if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                    continue;
                }
                return Ok(());
            }
            Err(err) => {
                let message = err.to_string();
                let summary = background_job_failure_summary(&message);
                let completed_at = now_ms();
                let finalization = finalize_failed_background_turn(
                    &store,
                    &state,
                    &ipc,
                    &message,
                    &summary,
                    completed_at,
                )?;
                match finalization {
                    BackgroundTurnFinalization::Queued => continue,
                    BackgroundTurnFinalization::Stopped => return Ok(()),
                    BackgroundTurnFinalization::CancelledOrSuperseded => {
                        if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                            continue;
                        }
                        return Ok(());
                    }
                    BackgroundTurnFinalization::Applied => {}
                }
                store.append_event(&job_id, "failed", serde_json::json!({ "error": message }))?;
                if wait_for_terminal_worker_reuse(store, job_id, ipc, handed_back).await? {
                    continue;
                }
                return Err(err);
            }
        }
    }
}

/// Apply a hook's session title the way the terminal does, minus the view
/// it has and this process does not: record it, then tell the viewers.
async fn publish_hook_session_title(session: &crate::EngineSession, title: String) {
    let title = title.trim().to_string();
    if title.is_empty() {
        return;
    }
    persist_session_title(session, &title);
    session
        .engine_half
        .update_publisher
        .publish_to(&session.session_id, session_title_update(title))
        .await;
}

/// Outcome of the end-of-turn attempt to acknowledge the claimed pending
/// prompts as durably answered. Anything but `Acked` after a successful
/// execution fails the turn so the claim is retained for a retry; the
/// variants exist so the failure message can say which guarantee broke.
enum CompletionAck {
    /// Transcript evidence checked out and the store recorded completion.
    Acked,
    /// Execution failed or was cancelled first; no acknowledgement is due.
    NotAttempted,
    /// The store no longer shows this worker owning the turn.
    OwnershipLost,
    /// The transcript on disk lacks a completed turn for the claimed
    /// prompts; carries the observed state for the error message.
    NotDurable(PendingPromptTranscriptState),
}

/// How long a worker's first turn waits for the MCP servers to come up. A
/// bound, because a server that never answers must not hold the turn
/// hostage; generous, because the alternative is a whole turn without tools.
const FIRST_MCP_LOAD_WAIT: Duration = Duration::from_secs(10);

/// Wait (bounded) for the session's MCP servers, list their tools once, and
/// publish what they look like for the session's mirrors.
///
/// The tool listing is what fills the cache `/mcp` and the snapshot read; a
/// turn would fill it too, but a hosted session's first `/mcp` usually comes
/// before its first turn.
async fn settle_mcp_and_publish(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    session: &mut crate::EngineSession,
) {
    if let Some(mcp) = session.engine_half.mcp.as_mut() {
        if mcp.wait_for_load(FIRST_MCP_LOAD_WAIT).await {
            mcp.warm_tool_cache().await;
        } else {
            tracing::warn!(
                job_id = %state.identity.job_id,
                wait_ms = FIRST_MCP_LOAD_WAIT.as_millis() as u64,
                "rebon: MCP servers were still loading when the turn started; running without them"
            );
        }
    }
    ipc.publish_mcp_status(
        store,
        &state.identity.job_id,
        crate::mcp_status::owner_mcp_snapshot(session),
    );
}

/// The held stack's load can land while no session is around to notice, and
/// a mirror asking `/mcp` in that gap would be told nothing. Publish it from
/// the stack itself, session or no session.
async fn publish_held_mcp_status(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    held_mcp: &mut Option<crate::mcp::SessionMcp>,
) {
    let Some(mcp) = held_mcp.as_mut() else {
        return;
    };
    let landed = mcp.drain_load_events().is_some();
    if landed {
        mcp.warm_tool_cache().await;
    }
    // Only when something happened: the snapshot reads the MCP configuration
    // off disk, and this runs once a second for as long as the worker idles.
    if !landed && ipc.has_mcp_status() {
        return;
    }
    ipc.publish_mcp_status(
        store,
        &state.identity.job_id,
        Some(crate::mcp_status::stack_snapshot(
            mcp,
            std::path::Path::new(&state.identity.cwd),
            &state.identity.runtime.mcp_configs,
            state.identity.runtime.strict_mcp_config,
        )),
    );
}

/// Record a finished turn's usage in both places it has to appear.
///
/// The session's ledger is what `/cost` and `/context` answer from when the
/// command runs here -- every counter, including the ones a mirror's snapshot
/// has no room for. The job record's snapshot is what a client that only
/// mirrors this session reads. One function so a later writer cannot update
/// one and forget the other.
/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn publish_turn_usage(
    session: &crate::EngineSession,
    store: &BackgroundStore,
    job_id: &str,
    turn_usage: &rebon_types::Usage,
) {
    session
        .engine_half
        .usage_ledger
        .lock()
        .expect("usage ledger poisoned")
        .add_turn(
            session.model.provider_name.clone(),
            session.model.name.clone(),
            *turn_usage,
        );
    if let Err(error) = store.update_state(job_id, |current| {
        current
            .outcome
            .usage
            .get_or_insert_with(Default::default)
            .add_turn(turn_usage);
        Ok(())
    }) {
        tracing::debug!(%job_id, %error, "could not publish session usage");
    }
}

/// Close the turn out: publish what it produced, then let the session go.
///
/// Everything here happens after the model is done and before the caller gets
/// its outcome. The order matters at two points: the usage total is published
/// before the turn is announced as ended, so a client reacting to `idle`
/// already sees it; and the session is released only after the MCP status is
/// published, because the release takes the servers with it.
#[allow(clippy::too_many_arguments)]
async fn finish_background_turn(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    ipc: &BackgroundIpcServer,
    session: &mut crate::EngineSession,
    turn_cancel: &rebon_types::PromptCancel,
    handed_back: &mut crate::session_handoff::HandedBack,
    worktree: BackgroundWorktreeGuard,
    execution: Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>,
    mut replay_requests: Vec<rebon_agent_core::DenialReplayRequest>,
    published_retry: Option<BackgroundRetryProgress>,
    command_inputs: WorkerCommandInputs,
    summary_client: Arc<dyn rebon_api::ModelClient>,
    summary_prompt: String,
    task_bridge_stop: tokio::sync::oneshot::Sender<()>,
    task_bridge: tokio::task::JoinHandle<()>,
) -> anyhow::Result<BackgroundExecutionOutcome> {
    // The owner is the only process that sees a turn's usage, so it is the
    // only one that can publish a running total. Do it before announcing the
    // turn ended, so a client reacting to `idle` reads the total including it.
    if let Ok(outcome) = execution.as_ref() {
        publish_turn_usage(session, store, &state.identity.job_id, &outcome.usage);
    }
    ipc.events.publish_turn(
        rebon_session_host::TurnStreamState::Idle,
        Some(match &execution {
            Ok(outcome) => serde_json::to_value(outcome.stop_reason)
                .expect("stop reason serializes")
                .as_str()
                .expect("stop reason is a string")
                .to_string(),
            Err(rebon_agent_core::PromptExecutorError::Cancelled) => "cancelled".to_string(),
            Err(error) => format!("error: {error}"),
        }),
    );
    if published_retry.is_some() {
        let mut published = published_retry;
        publish_retry_progress(
            store,
            &state.identity.job_id,
            &rebon_api::RetryNotifier::default(),
            &mut published,
        );
    }
    replay_requests.extend(drain_background_commands(
        ipc,
        &command_inputs,
        session,
        CommandDrainPhase::AfterTurn,
    ));
    if !replay_requests.is_empty() {
        if let Err(error) = execute_background_permission_replay(session, replay_requests).await {
            tracing::warn!(
                job_id = %state.identity.job_id,
                %error,
                "background permission replay command failed"
            );
        }
    }
    match store.read_state(&state.identity.job_id) {
        Ok(latest) if current_worker_owns_turn(&latest, state.process.turn_generation, ipc) => {
            *state = latest;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(
                job_id = %state.identity.job_id,
                turn_generation = state.process.turn_generation,
                %error,
                "background worker could not refresh pending prompts after execution"
            );
        }
    }
    if turn_cancel.is_cancelled() {
        worktree.preserve(store, state, "background worker was cancelled");
    } else if execution.is_ok() {
        // Integration may defer on a dirty/diverged source; that preserves
        // the worktree and records it on the job without failing the job.
        worktree.finish_success(store, state);
    } else {
        let reason = execution
            .as_ref()
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "background worker failed".to_string());
        worktree.preserve(store, state, &reason);
    }
    let completion_ack = if execution.is_ok() && !turn_cancel.is_cancelled() {
        let transcript_state =
            claimed_pending_prompt(state).map(|_| pending_prompt_transcript_state(state));
        match transcript_state {
            Some(transcript_state)
                if transcript_state != PendingPromptTranscriptState::Completed =>
            {
                CompletionAck::NotDurable(transcript_state)
            }
            _ => {
                if mark_claimed_pending_prompts_completed(store, state, ipc)? {
                    CompletionAck::Acked
                } else {
                    CompletionAck::OwnershipLost
                }
            }
        }
    } else {
        CompletionAck::NotAttempted
    };
    // Signal parent-prompt completion only. Do not await the bridge: detached
    // LocalAgents may finish later, and the bridge keeps draining until those
    // tasks go terminal (or the post-parent timeout). Dropping the JoinHandle
    // detaches the task in Tokio; it does not abort.
    let _ = task_bridge_stop.send(());
    let _ = task_bridge;
    if execution.is_ok() && !turn_cancel.is_cancelled() {
        let _ = refresh_agent_view_completion_summary(
            store,
            &state.identity.job_id,
            &summary_prompt,
            summary_client.as_ref(),
            &session.model.title_name,
        )
        .await;
    }
    // A load that landed during the turn, or a server the turn's `/mcp
    // disconnect` took down: say so before the session goes.
    if let Some(mcp) = session.engine_half.mcp.as_mut() {
        mcp.drain_load_events();
    }
    ipc.publish_mcp_status(
        store,
        &state.identity.job_id,
        crate::mcp_status::owner_mcp_snapshot(session),
    );
    release_session_keeping_lock(session, handed_back).await;
    if turn_cancel.is_cancelled() {
        return Ok(BackgroundExecutionOutcome::PromptCancelled);
    }
    if execution.is_ok() {
        match completion_ack {
            CompletionAck::Acked | CompletionAck::NotAttempted => {}
            CompletionAck::OwnershipLost => anyhow::bail!(
                "background turn finished, but the job was reassigned before its completion could be acknowledged"
            ),
            CompletionAck::NotDurable(transcript_state) => {
                let prompt_ids = claimed_pending_prompts(state)
                    .iter()
                    .map(|prompt| prompt.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let transcript_path = state
                    .identity
                    .session_id
                    .as_deref()
                    .map(|session_id| {
                        rebon_session::transcript_file_path(
                            &rebon_session::default_projects_root(),
                            &background_job_transcript_cwd(state),
                            session_id,
                        )
                        .display()
                        .to_string()
                    })
                    .unwrap_or_else(|| "unknown (no session bound)".to_string());
                anyhow::bail!(
                    "background prompt completed without a durable transcript response: {} (prompt {prompt_ids}, transcript {transcript_path})",
                    transcript_state.durability_gap(),
                );
            }
        }
    }
    execution
        .map(|_| BackgroundExecutionOutcome::PromptCompleted)
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// What running the turn produced.
struct BackgroundTurnRun {
    command_inputs: WorkerCommandInputs,
    published_retry: Option<BackgroundRetryProgress>,
    replay_requests: Vec<rebon_agent_core::DenialReplayRequest>,
    execution: Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError>,
}

/// The existing update task must drain the executor's published updates before
/// prompt completion samples the event cursor. Closing the receiver explicitly
/// avoids waiting for publishers still held by the warm engine session.
pub(crate) struct BackgroundUpdatePump {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl BackgroundUpdatePump {
    async fn finish(self) -> Result<(), rebon_agent_core::PromptExecutorError> {
        let _ = self.stop.send(());
        self.task.await.map_err(|error| {
            rebon_agent_core::PromptExecutorError::Execution(format!(
                "prompt update forwarding failed: {error}"
            ))
        })
    }
}

pub(crate) fn spawn_background_update_pump(
    mut updates: tokio::sync::mpsc::UnboundedReceiver<rebon_types::SessionUpdateParams>,
    mut publish: impl FnMut(rebon_types::SessionUpdateParams) + Send + 'static,
) -> BackgroundUpdatePump {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::pin!(stopped);
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => {
                    updates.close();
                    while let Some(update) = updates.recv().await {
                        publish(update);
                    }
                    break;
                }
                update = updates.recv() => match update {
                    Some(update) => publish(update),
                    None => break,
                },
            }
        }
    });
    BackgroundUpdatePump { stop, task }
}

/// Own the execution/completion boundary: steering must stop claiming queue
/// entries and updates must drain before the exact outcome reaches callers.
pub(crate) async fn execute_claimed_prompt(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    executor: &dyn rebon_agent_core::PromptExecutor,
    request: rebon_agent_core::PromptRequest,
    steer_pump: Option<BackgroundSteerPumpHandle>,
    update_pump: Option<BackgroundUpdatePump>,
) -> Result<rebon_agent_core::PromptOutcome, rebon_agent_core::PromptExecutorError> {
    let mut execution = executor.execute(request).await;
    if let Some(steer_pump) = steer_pump {
        steer_pump.stop().await;
    }
    if let Some(update_pump) = update_pump {
        if let Err(error) = update_pump.finish().await {
            execution = Err(error);
        }
    }
    ipc.complete_prompt_turn(store, state, &execution);
    if let Err(error) = &execution {
        if !matches!(error, rebon_agent_core::PromptExecutorError::Cancelled) {
            ipc.fail_prompt_calls(&error.to_string());
        }
    }
    execution
}

/// Run the turn, publishing progress while it runs.
///
/// The MCP stack has to be up before the turn lists its tools, and the turn
/// has to be announced before its first token can arrive, so both happen here
/// rather than at the call site. The retry poll exists because a turn waiting
/// out a rate limit produces nothing at all, which from a client is
/// indistinguishable from a hung turn.
#[allow(clippy::too_many_arguments)]
async fn run_background_turn(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    ipc: &BackgroundIpcServer,
    session: &mut crate::EngineSession,
    external_agent: &Option<String>,
    background_pending_prompt_poller: Arc<BackgroundPendingPromptPoller>,
    request: rebon_agent_core::PromptRequest,
    update_pump: BackgroundUpdatePump,
) -> BackgroundTurnRun {
    let command_inputs = WorkerCommandInputs::from_session(
        session,
        state
            .identity
            .runtime
            .ui_mode
            .as_deref()
            .and_then(|mode| mode.parse().ok())
            .unwrap_or_default(),
    );
    let steer_pump = external_agent.as_ref().map(|_| {
        spawn_background_steer_pump(
            session.engine_half.runtime.session_agents.clone(),
            background_pending_prompt_poller.clone(),
            session.engine_half.update_publisher.clone(),
            session.session_id.clone(),
        )
    });
    let executor = Arc::clone(&session.engine_half.executor);
    // The turn lists its tools once, as it starts, so the servers have to be
    // up by then or the model runs this turn without them. Only the worker's
    // first turn pays this wait: the stack is kept across turns, so every
    // later one finds it loaded.
    settle_mcp_and_publish(store, state, ipc, session).await;
    // Announce the turn before the first token can arrive, so a client shows
    // Stop from the moment the turn exists rather than from the moment it
    // produces something.
    ipc.events
        .publish_turn(rebon_session_host::TurnStreamState::Running, None);
    // And the record as it stands at the start of the turn — the model and
    // effort this turn runs under, above all, which the top of the turn may
    // just have re-read from a `/model` another client sent.
    ipc.publish_status_now(store, &state.identity.job_id);
    let mut execution_future = Box::pin(execute_claimed_prompt(
        store,
        state,
        ipc,
        executor.as_ref(),
        request,
        steer_pump,
        Some(update_pump),
    ));
    let mut replay_requests = Vec::new();
    // A turn waiting out a rate limit produces no output at all, and from a
    // client that is indistinguishable from a hung turn. The retry state lives
    // in a cell the model client writes and a UI reads each frame; a worker has
    // no frames, so poll it and publish the changes.
    let mut retry_poll = tokio::time::interval(RETRY_POLL_INTERVAL);
    retry_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut published_retry: Option<BackgroundRetryProgress> = None;
    let execution = loop {
        tokio::select! {
            result = &mut execution_future => break result,
            _ = retry_poll.tick() => {
                publish_retry_progress(
                    store,
                    &state.identity.job_id,
                    &session.engine_half.retry_notifier,
                    &mut published_retry,
                );
            }
            Some(request) = ipc.recv_command() => {
                replay_requests.extend(execute_background_command_request(
                    ipc,
                    &command_inputs,
                    session,
                    request,
                    CommandDrainPhase::DuringTurn,
                ));
                // One wakeup, every command that queued behind it.
                replay_requests.extend(drain_background_commands(
                    ipc,
                    &command_inputs,
                    session,
                    CommandDrainPhase::DuringTurn,
                ));
            }
        }
    };
    BackgroundTurnRun {
        command_inputs,
        published_retry,
        replay_requests,
        execution,
    }
}

/// Build the prompt request this turn will run.
///
/// On crash recovery the user row is already durable, so the prompt blocks are
/// deliberately empty and the ultrawork opt-in is detected against the
/// resolved durable text instead -- the reminder is only injected when the
/// user block is actually being sent.
#[allow(clippy::too_many_arguments)]
fn build_background_prompt_request(
    state: &BackgroundJobState,
    session: &crate::EngineSession,
    turn_cancel: &rebon_types::PromptCancel,
    prompt: String,
    prompt_images: &[BackgroundImageAttachment],
    thinking_overrides: crate::commands::effort::ThinkingOverrides,
    coordinator_report_paths: Vec<String>,
    pending_user_message_uuid: Option<String>,
    agent_system: Option<String>,
    agent_filter: Option<rebon_types::ToolFilterSpec>,
    resume_saved_user: bool,
) -> rebon_agent_core::PromptRequest {
    // 只分类已领取的用户原文，不把 hook 附加内容或后台报告当成首次请求。
    let user_prompt = claimed_pending_prompt(state)
        .filter(|pending| !resume_saved_user && pending.coordinator_report_paths.is_empty())
        .map(|pending| pending.text.clone());
    let durable_ultrawork_requested = rebon_acp::starts_with_ultrawork_command(&prompt);
    let skill_invocations = if resume_saved_user {
        // The user row is already durable; its skill, if it named one, was
        // dispatched by the turn that wrote it.
        Vec::new()
    } else {
        user_skill_invocations(session, &prompt)
    };
    let mut prompt_blocks = if resume_saved_user {
        Vec::new()
    } else {
        vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
            text: prompt,
            annotations: None,
        })]
    };
    if !resume_saved_user {
        prompt_blocks.extend(
            prompt_images
                .iter()
                .map(BackgroundImageAttachment::to_content_block),
        );
    }
    // Detect an ultrawork opt-in (`/ultrawork` / `/ulw`) and, when present,
    // inject the workflow-orchestration reminder + attach the workflow-controller
    // execution policy for this turn. Desktop chat runs through this background
    // worker (not the ACP `session/prompt` handler), so without this the command
    // was inert here — only the interactive TUI and standalone ACP surfaces wired
    // it. Shared helper keeps the reminder text identical across all three.
    // On crash recovery the user row is already durable, so `prompt_blocks` is
    // intentionally empty to avoid appending it twice. Detect against the
    // resolved durable text in that state, but only inject the reminder when the
    // user block is actually being sent.
    let (prompt_blocks, injected_ultrawork_reminder) =
        rebon_acp::acp_prompt_with_ultrawork_reminder(prompt_blocks);
    let ultrawork_requested = if resume_saved_user {
        durable_ultrawork_requested
    } else {
        injected_ultrawork_reminder
    };
    let request = rebon_agent_core::PromptRequest {
        user_prompt,
        effort_is_session_default: true,
        session_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        prompt: prompt_blocks,
        mcp_servers: Vec::new(),
        update_publisher: Some(session.engine_half.update_publisher.clone()),
        permission_publisher: None,
        cancel: turn_cancel.clone(),
        thinking_budget: thinking_overrides.thinking_budget,
        max_tokens: thinking_overrides.max_tokens,
        reasoning_effort_ordinal: thinking_overrides.reasoning_effort_ordinal,
        additional_working_directories: state.identity.runtime.add_dirs.clone(),
        coordinator_mode: Some(session.engine_half.coordinator_mode_handle.get()),
        coordinator_report_paths,
        user_message_uuid: (!resume_saved_user)
            .then_some(pending_user_message_uuid)
            .flatten(),
        background_agent_system: agent_system,
        background_agent_tool_filter: agent_filter,
        execution_policy: ultrawork_requested
            .then(rebon_types::ExecutionPolicy::workflow_controller),
        replay_requests: Vec::new(),
        skill_invocations,
    };
    request
}

/// What the turn's side channels leave behind for the finish step.
struct BackgroundTurnSideChannels {
    update_pump: BackgroundUpdatePump,
    pending_user_message_uuid: Option<String>,
    coordinator_report_paths: Vec<String>,
    task_bridge_stop: tokio::sync::oneshot::Sender<()>,
    task_bridge: tokio::task::JoinHandle<()>,
    summary_prompt: String,
    summary_client: Arc<dyn rebon_api::ModelClient>,
}

/// Start the turn's side channels: the task-store bridge and the agent-view
/// summary refresher.
///
/// The summary client is forked rather than shared: on the codex websocket
/// transport a concurrent side-channel request corrupts the
/// `previous_response_id` chain, and an abandoned one can leave its response
/// to be consumed by the main turn as `{"summary":...}` assistant text. One
/// isolated client is forked here and reused for every refresh in this run.
fn start_background_side_channels(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    session: &crate::EngineSession,
    prompt: &String,
    update_rx: tokio::sync::mpsc::UnboundedReceiver<rebon_types::SessionUpdateParams>,
    task_bridge_state: Option<BackgroundTaskBridgeState>,
) -> BackgroundTurnSideChannels {
    let pending_user_message_uuid = claimed_pending_prompt(state).map(|prompt| prompt.id.clone());
    let coordinator_report_paths = background_prompt_coordinator_report_paths(state);
    let (task_bridge_stop, task_bridge) = spawn_task_store_bridge_from_cursor(
        Arc::clone(&session.engine_half.tasks),
        store.clone(),
        state.identity.job_id.clone(),
        session.session_id.clone(),
        TaskEventCursor::ZERO,
        task_bridge_state,
    );
    let summary_prompt = prompt.clone();
    let update_store = store.clone();
    let update_job_id = state.identity.job_id.clone();
    let update_turn_generation = state.process.turn_generation;
    let update_summary_prompt = summary_prompt.clone();
    // Side-channel model calls (agent-view summaries) must never share
    // provider session state with the live conversation: on the codex
    // websocket transport a concurrent summary request corrupts the
    // previous_response_id chain, and an abandoned one can leave its
    // response to be consumed by the main turn as `{"summary":...}`
    // assistant text. Fork an isolated client (same auth + middleware,
    // fresh session state and connection) and reuse it for every
    // summary refresh in this execution.
    let summary_client: Arc<dyn rebon_api::ModelClient> = session
        .model
        .runtime_model
        .session()
        .fork_for_sub_agent(None)
        .client_arc();
    let update_summary_client = Arc::clone(&summary_client);
    let update_summary_model = session.model.title_name.clone();
    let update_events = ipc.events.clone();
    let mut last_model_summary_refresh_ms = None;
    let update_pump = spawn_background_update_pump(update_rx, move |update| {
        // Push before the file write. The log stays as the Agent View's
        // summary source and as crash forensics; this is what a live
        // client reads, and it must not wait on a disk round trip to see
        // a token the owner already has.
        // The log line carries the cursor the stream gave this update,
        // so a client that takes deltas live and reads the file only to
        // fill a gap can tell the two apart.
        let _ = match update_events.publish_update_for_turn(&update, update_turn_generation) {
            Some(stamp) => update_store.append_session_update_for_turn_at(
                &update_job_id,
                update_turn_generation,
                &update,
                stamp,
            ),
            None => update_store.append_session_update_for_turn(
                &update_job_id,
                update_turn_generation,
                &update,
            ),
        };
        let now = now_ms();
        if !should_refresh_agent_view_model_summary(&mut last_model_summary_refresh_ms, now) {
            return;
        }
        let summary_store = update_store.clone();
        let summary_job_id = update_job_id.clone();
        let summary_prompt = update_summary_prompt.clone();
        let summary_client = Arc::clone(&update_summary_client);
        let summary_model = update_summary_model.clone();
        tokio::spawn(async move {
            let _ = refresh_agent_view_running_summary(
                summary_store,
                summary_job_id,
                summary_prompt,
                summary_client,
                summary_model,
            )
            .await;
        });
    });
    BackgroundTurnSideChannels {
        update_pump,
        pending_user_message_uuid,
        coordinator_report_paths,
        task_bridge_stop,
        task_bridge,
        summary_prompt,
        summary_client,
    }
}

/// Settle a resume-only wake before any turn runs.
///
/// A resume-only job is a warm session with nothing to say yet: it waits for
/// work, repairs a half-written transcript if it finds one, or ends. `Some`
/// means the job is done for this wake; `None` means a real turn follows.
#[allow(clippy::too_many_arguments)]
async fn resolve_resume_only_turn(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    ipc: &BackgroundIpcServer,
    session: &mut crate::EngineSession,
    turn_cancel: &mut rebon_types::PromptCancel,
    handed_back: &mut crate::session_handoff::HandedBack,
    pending_transcript_state: &mut PendingPromptTranscriptState,
    resume_saved_user: &mut bool,
) -> anyhow::Result<Option<BackgroundExecutionOutcome>> {
    if state.identity.resume_only {
        let idle_at = now_ms();
        store.update_state(&state.identity.job_id, |current| {
            current.identity.resume_only = false;
            current.process.status = if current.has_pending_prompts() {
                BackgroundJobStatus::Queued
            } else {
                BackgroundJobStatus::Idle
            };
            current.outcome.pending_permission = None;
            current.outcome.summary_updated_at_ms = None;
            current.process.completed_at_ms = None;
            current.outcome.exit_code = None;
            current.outcome.error = None;
            current.process.updated_at_ms = idle_at;
            Ok(())
        })?;
        store.append_event(
            &state.identity.job_id,
            "worker_warmed_for_peek",
            serde_json::json!({ "sessionId": session.session_id }),
        )?;
        // A warmed worker is what `/hosted` hands a session to, and the
        // terminal that handed it over is about to ask `/mcp`. Say what
        // the servers are before there is a turn to say it for.
        settle_mcp_and_publish(store, state, ipc, session).await;
        // The session stays up for the first prompt.
        // Letting it go and rebuilding it when the prompt arrived was most
        // of what a hosted session's first token waited for beyond a local
        // one — the local engine is built before the user can type, and
        // this is the same engine, so it should be too.
        match wait_for_first_prompt_on_warm_session(store, state, ipc, session).await? {
            WarmSessionWait::Claimed { claimed, cancel } => {
                *state = *claimed;
                *turn_cancel = cancel;
                ipc.retarget_permission_receivers(state.process.turn_generation);
                store.append_event(
                    &state.identity.job_id,
                    "worker_started",
                    serde_json::json!({ "pid": state.process.pid, "warmSession": true }),
                )?;
                // The same classification the cold path made at the top, for
                // the prompt that was just claimed rather than the warm-up.
                *pending_transcript_state = pending_prompt_transcript_state(state);
                if matches!(
                    &*pending_transcript_state,
                    PendingPromptTranscriptState::ResumeInterruptedToolUse { .. }
                ) {
                    let repaired_tool_uses =
                        persist_interrupted_tool_results(state, pending_transcript_state)?;
                    store.append_event(
                        &state.identity.job_id,
                        "pending_prompt_tool_results_repaired",
                        serde_json::json!({
                            "promptId": claimed_pending_prompt(state).map(|prompt| &prompt.id),
                            "toolUses": repaired_tool_uses,
                        }),
                    )?;
                    *pending_transcript_state = pending_prompt_transcript_state(state);
                }
                if *pending_transcript_state == PendingPromptTranscriptState::Completed {
                    release_session_keeping_lock(session, handed_back).await;
                    if !mark_claimed_pending_prompts_completed(store, state, ipc)? {
                        return Ok(Some(BackgroundExecutionOutcome::PromptCancelled));
                    }
                    return Ok(Some(BackgroundExecutionOutcome::PromptAlreadyPersisted));
                }
                if matches!(
                    &pending_transcript_state,
                    PendingPromptTranscriptState::ResumeInterruptedToolUse { .. }
                ) {
                    release_session_keeping_lock(session, handed_back).await;
                    anyhow::bail!(
                        "background prompt tool-result repair did not produce a resumable transcript"
                    );
                }
                *resume_saved_user =
                    *pending_transcript_state == PendingPromptTranscriptState::ResumeSavedUser;
            }
            WarmSessionWait::Ended => {
                release_session_keeping_lock(session, handed_back).await;
                return Ok(Some(BackgroundExecutionOutcome::IdleWarmEnded));
            }
        }
    }
    Ok(None)
}

/// Whether the `UserPromptSubmit` hook let this turn through.
///
/// `Continue` hands the worktree guard back because the refusal path consumes
/// it, and a guard that is not consumed has to reach the caller intact.
enum PromptSubmitDecision {
    Refused(BackgroundExecutionOutcome),
    Continue(BackgroundWorktreeGuard),
}

/// Run the turn's `UserPromptSubmit` hook.
///
/// The hook runs where the turn runs. A terminal that mirrors this session
/// sends the prompt over without running it, so a hook that annotates or
/// refuses a prompt would otherwise be skipped for every hosted session --
/// which is every session by default. A recovered turn's
/// user row is already durable: its hook ran when the row was written.
/// `Some` means the prompt was refused and the job is done.
#[allow(clippy::too_many_arguments)]
async fn run_user_prompt_submit_hook(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    session: &mut crate::EngineSession,
    worktree: BackgroundWorktreeGuard,
    handed_back: &mut crate::session_handoff::HandedBack,
    prompt: &mut String,
    resume_saved_user: bool,
) -> anyhow::Result<PromptSubmitDecision> {
    if !resume_saved_user {
        let verdict = session
            .engine_half
            .runtime
            .policy
            .emit(
                rebon_core::policy_seat::HookEventPayload::UserPromptSubmit {
                    prompt: prompt.clone(),
                },
            )
            .await;
        let decision = match verdict.denial() {
            // A gated event's terminal refusal is this prompt's refusal.
            Some(reason) => UserPromptSubmitDecision::Blocked {
                reason: reason.to_string(),
            },
            None => apply_user_prompt_submit_effects(verdict.effects()),
        };
        match decision {
            UserPromptSubmitDecision::Blocked { reason } => {
                // Nothing was written for this turn, and a session
                // that never had a row has no file — the next prompt
                // would ask this worker to resume a session it
                // cannot find.
                if let Err(err) = session.ensure_transcript_on_disk() {
                    tracing::warn!(
                        job_id = %state.identity.job_id,
                        error = %err,
                        "could not materialise the refused session's transcript"
                    );
                }
                release_session_keeping_lock(session, handed_back).await;
                worktree.preserve(store, state, "a UserPromptSubmit hook refused the prompt");
                return Ok(PromptSubmitDecision::Refused(
                    BackgroundExecutionOutcome::PromptRefused { reason },
                ));
            }
            UserPromptSubmitDecision::Continue(effects) => {
                for context_text in &effects.additional_context {
                    append_additional_context(prompt, context_text);
                }
                for text in &effects.system_messages {
                    tracing::info!(
                        job_id = %state.identity.job_id,
                        message = %text,
                        "UserPromptSubmit hook message"
                    );
                }
                if let Some(title) = effects.session_title {
                    publish_hook_session_title(session, title).await;
                }
                if effects.mark_session_complete {
                    let title = completed_session_title_for(session);
                    publish_hook_session_title(session, title).await;
                }
            }
        }
    }
    Ok(PromptSubmitDecision::Continue(worktree))
}

/// Whether the session this turn needs could be built and bound.
///
/// `Ready` hands back everything the turn owns from here on, the worktree
/// guard included: the cancelled path consumes the guard, so a guard that
/// survives has to travel back with the session.
enum BackgroundSessionSetup {
    Cancelled(BackgroundExecutionOutcome),
    Ready {
        session: crate::EngineSession,
        worktree: BackgroundWorktreeGuard,
        update_rx: tokio::sync::mpsc::UnboundedReceiver<rebon_types::SessionUpdateParams>,
        task_bridge_state: Option<BackgroundTaskBridgeState>,
        background_pending_prompt_poller: Arc<BackgroundPendingPromptPoller>,
    },
}

/// Build the session this turn runs in and bind it to the job.
///
/// The owner descriptor is published once the lock is held and the endpoint is
/// up, and before the first transcript write: a client that can see the writes
/// but not the writer has no way to reach it. The update and permission
/// channels are swapped out of the session here so the IPC server, not the
/// session, is what clients talk to.
#[allow(clippy::too_many_arguments)]
async fn build_background_session(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    ipc: &BackgroundIpcServer,
    turn_cancel: &rebon_types::PromptCancel,
    handed_back: &mut crate::session_handoff::HandedBack,
    worktree: BackgroundWorktreeGuard,
    resume_cwd: Option<String>,
) -> anyhow::Result<BackgroundSessionSetup> {
    let effective_cwd = resume_cwd.unwrap_or_else(|| {
        worktree
            .path()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| state.identity.cwd.clone())
    });

    let mut overrides = state.identity.runtime.to_runtime_override()?;
    overrides.cwd = Some(effective_cwd.clone());
    overrides.queue_session = state.identity.queue_session;
    if let Some(session_id) = state.identity.session_id.clone() {
        overrides.resume = Some(session_id);
    }
    let background_pending_prompt_poller =
        BackgroundPendingPromptPoller::new(store.clone(), state, ipc);
    let background_attachment_poller: Arc<dyn AttachmentPoller> =
        background_pending_prompt_poller.clone();
    let task_runtime_controller =
        ipc.task_runtime_controller(store.clone(), state.identity.job_id.clone());
    // A worker has no screen, so it keeps the session and drops the
    // terminal's view fields the builder hands back with it.
    let mut session = crate::build::build_session_with_runtime_controller(
        overrides,
        Some(background_attachment_poller),
        Some(ipc.shared_auto_mode_state.clone()),
        state.identity.runtime.capability_mode,
        Some(task_runtime_controller),
        std::mem::take(handed_back),
    )
    .await?
    .session;
    session.cwd = effective_cwd.clone();
    ipc.attach_permission_mode_cell(Arc::clone(&session.engine_half.permission_mode_cell));
    ipc.apply_pending_context_commands(&session);
    let session_id = session.session_id.clone();
    let job_id_for_update = state.identity.job_id.clone();
    let session_bound = store.update_state(&job_id_for_update, |current| {
        if !current_worker_owns_turn(current, state.process.turn_generation, ipc)
            || !matches!(
                current.process.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
            )
        {
            return Ok(false);
        }
        current.identity.session_id = Some(session_id.clone());
        current.process.updated_at_ms = now_ms();
        Ok(true)
    })?;
    if !session_bound || turn_cancel.is_cancelled() {
        release_session_keeping_lock(&mut session, handed_back).await;
        worktree.preserve(
            store,
            state,
            "background worker did not complete successfully",
        );
        return Ok(BackgroundSessionSetup::Cancelled(
            BackgroundExecutionOutcome::PromptCancelled,
        ));
    }
    if crate::host::should_hide_agent_session_from_chats(state.identity.agent_type.as_deref()) {
        if let Err(error) = rebon_session::save_session_hidden_from_chats(
            &rebon_session::default_projects_root(),
            &effective_cwd,
            &session.session_id,
            true,
        ) {
            tracing::warn!(
                error = %error,
                session_id = %session.session_id,
                "failed to hide verification session from Chats"
            );
        }
    }
    ipc.attach_task_registry_resolver(session.engine_half.kernel_scopes.task_registry_resolver());
    let task_bridge_state = if let Some(team_manager) = session.engine_half.team_manager.clone() {
        ipc.attach_teammate_runtime(
            Arc::clone(&session.engine_half.tasks),
            team_manager,
            session.session_id.clone(),
        )
    } else {
        None
    };
    state.identity.session_id = Some(session.session_id.clone());
    state.process.updated_at_ms = now_ms();
    // Publish where this session can be reached, now that the lock is held and
    // the endpoint is up, and before the first write. A client that can see the
    // writes but not the writer has no way to reach it, so this is a gate: `?`
    // ends the build here, the session drops with its lock, and the job lands
    // in the terminal finalizer as `Failed`.
    publish_session_owner_descriptor(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
        &state.identity.job_id,
        ipc,
    )?;
    store.append_event(
        &state.identity.job_id,
        "session_started",
        serde_json::json!({ "sessionId": session.session_id }),
    )?;

    let (_unused_update_publisher, replacement_update_rx) =
        rebon_agent_core::ChannelSessionUpdatePublisher::new();
    let update_rx = std::mem::replace(&mut session.engine_half.update_rx, replacement_update_rx);
    let (_unused_broker, replacement_permission_rx) =
        rebon_core::permission::ChannelPermissionBroker::new(session.session_id.clone());
    let permission_rx = std::mem::replace(
        &mut session.engine_half.permission_rx,
        replacement_permission_rx,
    );
    // The profile gate goes in front of the IPC hand-off rather than behind
    // it: whoever opens this job has to see the diff computed from the live
    // session, and an approval has to come back past a point that still holds
    // the session. Behind the hand-off there is no such point, which is why a
    // profile approved on a worker used to apply nothing at all.
    let permission_rx =
        crate::profile_proposal::spawn_profile_proposal_relay(&session, permission_rx);
    ipc.attach_permission_receiver(state.process.turn_generation, permission_rx);
    ipc.attach_permission_rule_context(
        effective_cwd.clone(),
        session.engine_half.live_policy_store.clone(),
    );
    Ok(BackgroundSessionSetup::Ready {
        session,
        worktree,
        update_rx,
        task_bridge_state,
        background_pending_prompt_poller,
    })
}

pub(crate) async fn execute_background_job(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    ipc: &BackgroundIpcServer,
    turn_cancel: rebon_types::PromptCancel,
    handed_back: &mut crate::session_handoff::HandedBack,
) -> anyhow::Result<BackgroundExecutionOutcome> {
    struct ExecutionGuard<'a>(&'a BackgroundIpcServer, bool);
    impl Drop for ExecutionGuard<'_> {
        fn drop(&mut self) {
            if self.1 {
                self.0
                    .fail_prompt_calls("prompt startup or execution was abandoned");
            }
        }
    }
    let mut guard = ExecutionGuard(ipc, true);
    let result = execute_background_job_inner(store, state, ipc, turn_cancel, handed_back).await;
    match &result {
        Err(error) => ipc.fail_prompt_calls(&error.to_string()),
        Ok(BackgroundExecutionOutcome::PromptCancelled) => ipc.complete_prompt_turn(
            store,
            state,
            &Err(rebon_agent_core::PromptExecutorError::Cancelled),
        ),
        Ok(BackgroundExecutionOutcome::PromptRefused { .. }) => ipc.complete_prompt_turn(
            store,
            state,
            &Ok(rebon_agent_core::PromptOutcome {
                stop_reason: rebon_types::StopReason::Refusal,
                ..rebon_agent_core::PromptOutcome::end_turn()
            }),
        ),
        Ok(BackgroundExecutionOutcome::PromptAlreadyPersisted) => ipc.complete_prompt_turn(
            store,
            state,
            &Err(rebon_agent_core::PromptExecutorError::Execution(
                "the persisted turn has no retained execution result".into(),
            )),
        ),
        Ok(BackgroundExecutionOutcome::IdleWarmEnded) => {
            ipc.fail_prompt_calls("prompt host stopped before execution")
        }
        Ok(BackgroundExecutionOutcome::PromptCompleted) => {}
    }
    guard.1 = false;
    result
}

async fn execute_background_job_inner(
    store: &BackgroundStore,
    state: &mut BackgroundJobState,
    ipc: &BackgroundIpcServer,
    mut turn_cancel: rebon_types::PromptCancel,
    handed_back: &mut crate::session_handoff::HandedBack,
) -> anyhow::Result<BackgroundExecutionOutcome> {
    if turn_cancel.is_cancelled() {
        return Ok(BackgroundExecutionOutcome::PromptCancelled);
    }
    let mut pending_transcript_state = pending_prompt_transcript_state(state);
    if matches!(
        &pending_transcript_state,
        PendingPromptTranscriptState::ResumeInterruptedToolUse { .. }
    ) {
        let repaired_tool_uses =
            persist_interrupted_tool_results(state, &pending_transcript_state)?;
        store.append_event(
            &state.identity.job_id,
            "pending_prompt_tool_results_repaired",
            serde_json::json!({
                "promptId": claimed_pending_prompt(state).map(|prompt| &prompt.id),
                "toolUses": repaired_tool_uses,
            }),
        )?;
        pending_transcript_state = pending_prompt_transcript_state(state);
    }
    if pending_transcript_state == PendingPromptTranscriptState::Completed {
        if !mark_claimed_pending_prompts_completed(store, state, ipc)? {
            return Ok(BackgroundExecutionOutcome::PromptCancelled);
        }
        store.append_event(
            &state.identity.job_id,
            "pending_prompt_already_persisted",
            serde_json::json!({
                "promptId": claimed_pending_prompt(state).map(|prompt| &prompt.id),
            }),
        )?;
        return Ok(BackgroundExecutionOutcome::PromptAlreadyPersisted);
    }
    if matches!(
        &pending_transcript_state,
        PendingPromptTranscriptState::ResumeInterruptedToolUse { .. }
    ) {
        anyhow::bail!(
            "background prompt tool-result repair did not produce a resumable transcript"
        );
    }
    let mut resume_saved_user =
        pending_transcript_state == PendingPromptTranscriptState::ResumeSavedUser;
    let resume_cwd = if state.identity.resume_only {
        Some(state.identity.cwd.clone())
    } else {
        state
            .identity
            .session_id
            .as_ref()
            .filter(|_| !state.workspace.isolate_in_worktree)
            .map(|_| state.identity.cwd.clone())
    };
    // An isolated job whose worktree cannot be prepared — or can no longer be
    // reopened — fails here rather than below: the error reaches the caller
    // before the session is built and the model prompted, so the turn
    // finalizes as failed instead of running in the source checkout and
    // merging its work straight onto the source branch.
    let worktree = if resume_cwd.is_some() {
        BackgroundWorktreeGuard::none(None)
    } else {
        prepare_background_worktree(store, state)?
    };
    let (mut session, worktree, update_rx, task_bridge_state, background_pending_prompt_poller) =
        match build_background_session(
            store,
            state,
            ipc,
            &turn_cancel,
            handed_back,
            worktree,
            resume_cwd,
        )
        .await?
        {
            BackgroundSessionSetup::Cancelled(outcome) => return Ok(outcome),
            BackgroundSessionSetup::Ready {
                session,
                worktree,
                update_rx,
                task_bridge_state,
                background_pending_prompt_poller,
            } => (
                session,
                worktree,
                update_rx,
                task_bridge_state,
                background_pending_prompt_poller,
            ),
        };
    // The turn's Stop event runs here, where the turn runs, when a client
    // asks to cancel it: `BlockStop` refuses the cancel, and the reason
    // travels back as the request's failure. Attached per
    // turn, because the session — and its policy handle — is rebuilt per turn.
    {
        let handle = tokio::runtime::Handle::current();
        let policy = session.engine_half.runtime.policy.clone();
        ipc.attach_stop_gate(Arc::new(move |stop_reason: &str| {
            let verdict = handle.block_on(policy.emit(
                rebon_core::policy_seat::HookEventPayload::Stop {
                    stop_reason: Some(stop_reason.to_string()),
                },
            ));
            // Gated: a terminal refusal refuses the cancel, exactly as a
            // `BlockStop` effect does.
            match verdict.denial() {
                Some(reason) => Err(reason.to_string()),
                None => rebon_core::hooks::apply_stop_effects(verdict.effects()),
            }
        }));
    }

    if let Some(outcome) = resolve_resume_only_turn(
        store,
        state,
        ipc,
        &mut session,
        &mut turn_cancel,
        handed_back,
        &mut pending_transcript_state,
        &mut resume_saved_user,
    )
    .await?
    {
        return Ok(outcome);
    }

    if turn_cancel.is_cancelled() {
        release_session_keeping_lock(&mut session, handed_back).await;
        worktree.preserve(
            store,
            state,
            "background worker did not complete successfully",
        );
        return Ok(BackgroundExecutionOutcome::PromptCancelled);
    }

    // Before the effort level, because switching the model can switch the
    // provider under it and the thinking overrides below are resolved from
    // that provider's format.
    apply_recorded_model(&mut session, state, store).await;

    // Say which agent this turn runs under. The router lives in this process
    // and nothing asked it before, so `agent` was `None` in every snapshot the
    // owner ever sent and clients had to guess — which is how `serve` ended up
    // keeping its own copy of a value the owner was supposed to hold. Published
    // here because this is where the choice is settled for the turn: a
    // `/backend` between turns lands on the next one, the same as the model.
    ipc.publish_agent(
        store,
        &state.identity.job_id,
        Some(session.engine_half.runtime.session_agents.current_id()),
    );

    let effort_level = effort_level_from_runtime(&state.identity.runtime)?;
    let thinking_overrides = crate::commands::effort::resolve_thinking_from_effort(
        effort_level,
        provider_kind_from_format(session.model.provider_format),
    );
    let BackgroundAgentRuntime {
        mut prompt,
        system: agent_system,
        tool_filter: agent_filter,
        external_agent,
    } = resolve_background_agent_runtime(state);
    // The turn's UserPromptSubmit runs where the turn runs. A terminal that
    // mirrors this session sent the prompt over without running it, so a
    // hook that annotates or refuses a prompt would otherwise be skipped
    // for every hosted session — which is every session by default.
    // A recovered turn's user row is already durable:
    // its hook ran when the row was written.
    let worktree = match run_user_prompt_submit_hook(
        store,
        state,
        &mut session,
        worktree,
        handed_back,
        &mut prompt,
        resume_saved_user,
    )
    .await?
    {
        PromptSubmitDecision::Refused(outcome) => return Ok(outcome),
        PromptSubmitDecision::Continue(worktree) => worktree,
    };
    // Borrowed out of `state`, so it is taken after the hook above has had its
    // mutable turn; nothing between there and here touches the attachments.
    let prompt_images = background_prompt_images_for_execution(state);

    // A job launched with an external agent runs *on* that agent rather
    // than handing its (empty) system prompt to the local engine. This
    // is also what puts it in Agent View as itself: the job already has
    // a row and a session id, and the turns the agent produces land in
    // that session's transcript, so opening the row shows the real
    // conversation.
    if let Some(agent_id) = external_agent.as_deref() {
        if let Err(err) = session
            .engine_half
            .runtime
            .session_agents
            .switch_to(agent_id)
        {
            // Falling through would run Rebon's own loop with no system
            // prompt under this agent's name — a different agent than
            // the caller asked for, failing silently.
            release_session_keeping_lock(&mut session, handed_back).await;
            worktree.preserve(
                store,
                state,
                "background worker did not complete successfully",
            );
            anyhow::bail!(
                "background job requested agent `{agent_id}`, which is unavailable: {err}"
            );
        }
        store.append_event(
            &state.identity.job_id,
            "agent_backend_selected",
            serde_json::json!({ "agent": agent_id }),
        )?;
    }
    apply_background_ceo_command(&mut session, &mut prompt);
    let BackgroundTurnSideChannels {
        update_pump,
        pending_user_message_uuid,
        coordinator_report_paths,
        task_bridge_stop,
        task_bridge,
        summary_prompt,
        summary_client,
    } = start_background_side_channels(
        store,
        state,
        ipc,
        &session,
        &prompt,
        update_rx,
        task_bridge_state,
    );
    let request = build_background_prompt_request(
        state,
        &session,
        &turn_cancel,
        prompt,
        prompt_images,
        thinking_overrides,
        coordinator_report_paths,
        pending_user_message_uuid,
        agent_system,
        agent_filter,
        resume_saved_user,
    );
    let BackgroundTurnRun {
        command_inputs,
        published_retry,
        replay_requests,
        execution,
    } = run_background_turn(
        store,
        state,
        ipc,
        &mut session,
        &external_agent,
        background_pending_prompt_poller,
        request,
        update_pump,
    )
    .await;
    finish_background_turn(
        store,
        state,
        ipc,
        &mut session,
        &turn_cancel,
        handed_back,
        worktree,
        execution,
        replay_requests,
        published_retry,
        command_inputs,
        summary_client,
        summary_prompt,
        task_bridge_stop,
        task_bridge,
    )
    .await
}

/// End this turn's session while keeping its lock for the next one.
///
/// A worker outlives its sessions — it builds one per turn — so a lock that
/// went out of scope with the session would make ownership a property of
/// "a turn is running" rather than of the worker. Between turns the session
/// would read as free to every other process, while this worker was still
/// alive, still answering its endpoint, and still able to be woken by a
/// `Reply` into writing that very transcript. See
/// [`rebon_session::HeldSessionLock`].
///
/// The MCP stack comes back the same way. Before this it was torn down with
/// the session and rebuilt — servers and all — for the next turn, and since
/// nothing here ever collected the loader's result, no turn ever saw an MCP
/// tool. See [`crate::mcp::SessionMcp`].
async fn release_session_keeping_lock(
    session: &mut crate::EngineSession,
    handed_back: &mut crate::session_handoff::HandedBack,
) {
    if let Some(lock) = session.session_active_lock.take() {
        handed_back.session_lock = Some(rebon_session::HeldSessionLock {
            session_id: session.session_id.clone(),
            lock,
        });
    }
    if let Some(mcp) = session.engine_half.mcp.take() {
        // The holder was emptied when this session borrowed it; anything
        // still there is a stack nobody will reuse, and it must not leak.
        if let Some(orphaned) = handed_back.mcp.replace(mcp) {
            orphaned.shutdown().await;
        }
    }
    // Keep the kernel scope table across worker turns. The next build acquires
    // this exact session generation, so every task consumer resolves the same
    // registry seat instead of reconstructing registry identity at handoff.
    handed_back.kernel_scopes = Some(crate::session_handoff::HeldKernelScopes {
        session_id: session.session_id.clone(),
        scopes: Arc::clone(&session.engine_half.kernel_scopes),
    });
}

pub(crate) async fn refresh_agent_view_completion_summary(
    store: &BackgroundStore,
    job_id: &str,
    prompt: &str,
    client: &dyn rebon_api::ModelClient,
    model: &str,
) -> anyhow::Result<()> {
    let summary = generate_agent_view_summary_for_job(store, job_id, prompt, client, model).await;
    let Some(summary) = summary else {
        return Ok(());
    };

    let updated_at = now_ms();
    store.update_state(job_id, |state| {
        state.outcome.summary = Some(summary.clone());
        state.outcome.summary_updated_at_ms = None;
        state.process.updated_at_ms = updated_at;
        Ok(())
    })?;
    store.append_event(
        job_id,
        "agent_view_summary_updated",
        serde_json::json!({ "summary": summary, "phase": "completion" }),
    )?;
    Ok(())
}

pub(crate) async fn refresh_agent_view_running_summary(
    store: BackgroundStore,
    job_id: String,
    prompt: String,
    client: Arc<dyn rebon_api::ModelClient>,
    model: String,
) -> anyhow::Result<()> {
    let summary =
        generate_agent_view_summary_for_job(&store, &job_id, &prompt, client.as_ref(), &model)
            .await;
    let Some(summary) = summary else {
        return Ok(());
    };

    let updated_at = now_ms();
    let updated = store.update_state(&job_id, |state| {
        if state.process.status != BackgroundJobStatus::Running {
            return Ok(false);
        }
        state.outcome.summary = Some(summary.clone());
        state.outcome.summary_updated_at_ms = Some(updated_at);
        state.process.updated_at_ms = updated_at;
        Ok(true)
    })?;
    if updated {
        store.append_event(
            &job_id,
            "agent_view_summary_updated",
            serde_json::json!({ "summary": summary, "phase": "running" }),
        )?;
    }
    Ok(())
}

pub(crate) async fn generate_agent_view_summary_for_job(
    store: &BackgroundStore,
    job_id: &str,
    prompt: &str,
    client: &dyn rebon_api::ModelClient,
    model: &str,
) -> Option<String> {
    let input = background_job_agent_view_summary_input_from_store(store, job_id, prompt)?;
    let timeout = Duration::from_millis(BACKGROUND_AGENT_VIEW_SUMMARY_TIMEOUT_MS);
    tokio::time::timeout(
        timeout,
        rebon_api::generate_agent_view_summary(client, model, &input),
    )
    .await
    .ok()
    .flatten()
}

pub(crate) async fn process_idle_background_commands(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    handed_back: &mut crate::session_handoff::HandedBack,
) {
    let Some(first) = ipc.try_recv_command() else {
        return;
    };
    let Some(session_id) = state.identity.session_id.clone() else {
        let error = (
            rebon_session_host::HostCallError::SessionFailure,
            "background job has no session for commands".to_string(),
        );
        let _ = first.response_tx.send(Err(error.clone()));
        while let Some(request) = ipc.try_recv_command() {
            let _ = request.response_tx.send(Err(error.clone()));
        }
        return;
    };

    let session_result = async {
        let mut overrides = state.identity.runtime.to_runtime_override()?;
        overrides.cwd = Some(state.identity.cwd.clone());
        overrides.resume = Some(session_id);
        let task_runtime_controller =
            ipc.task_runtime_controller(store.clone(), state.identity.job_id.clone());
        let build = crate::build::build_session_with_runtime_controller(
            overrides,
            None,
            Some(ipc.shared_auto_mode_state.clone()),
            state.identity.runtime.capability_mode,
            Some(task_runtime_controller),
            std::mem::take(handed_back),
        )
        .await?;
        anyhow::Ok(build.session)
    }
    .await;
    let mut session = match session_result {
        Ok(session) => session,
        Err(error) => {
            let error = (
                rebon_session_host::HostCallError::SessionFailure,
                error.to_string(),
            );
            let _ = first.response_tx.send(Err(error.clone()));
            while let Some(request) = ipc.try_recv_command() {
                let _ = request.response_tx.send(Err(error.clone()));
            }
            return;
        }
    };
    run_idle_background_commands(store, state, ipc, &mut session, first).await;
    release_session_keeping_lock(&mut session, handed_back).await;
}

/// Run `first`, and whatever queued behind it, against a session that is up.
async fn run_idle_background_commands(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    session: &mut crate::EngineSession,
    first: BackgroundCommandRequest,
) {
    session.cwd = state.identity.cwd.clone();
    ipc.attach_permission_mode_cell(Arc::clone(&session.engine_half.permission_mode_cell));
    // `/mcp` and `/context` report the servers; collect a load that finished
    // while nobody was looking so they report what is actually there.
    if let Some(mcp) = session.engine_half.mcp.as_mut() {
        mcp.drain_load_events();
    }
    let command_inputs = WorkerCommandInputs::from_session(
        session,
        state
            .identity
            .runtime
            .ui_mode
            .as_deref()
            .and_then(|mode| mode.parse().ok())
            .unwrap_or_default(),
    );
    let mut replay_requests = execute_background_command_request(
        ipc,
        &command_inputs,
        session,
        first,
        CommandDrainPhase::Idle,
    );
    replay_requests.extend(drain_background_commands(
        ipc,
        &command_inputs,
        session,
        CommandDrainPhase::Idle,
    ));
    if !replay_requests.is_empty() {
        if let Err(error) = execute_background_permission_replay(session, replay_requests).await {
            tracing::warn!(
                job_id = %state.identity.job_id,
                %error,
                "idle background permission replay command failed"
            );
        }
    }
    // After the commands, not before: `/mcp disconnect` is one of them.
    ipc.publish_mcp_status(
        store,
        &state.identity.job_id,
        crate::mcp_status::owner_mcp_snapshot(session),
    );
}

/// What a warm session's wait ended with.
enum WarmSessionWait {
    /// A prompt was claimed; the session on hand runs it.
    Claimed {
        claimed: Box<BackgroundJobState>,
        cancel: rebon_types::PromptCancel,
    },
    /// Nothing more for this worker: stopped, replaced, or lingered out.
    Ended,
}

/// Wait, with the session still up, for its first prompt — or for the reason
/// to leave.
///
/// The counterpart of [`wait_for_terminal_worker_reuse`] for a worker that
/// has not let its session go: the same tick and wake, the same exits, but
/// a prompt is claimed here and handed back with the session, instead of
/// the loop returning to rebuild one. Idle commands run against this
/// session too.
async fn wait_for_first_prompt_on_warm_session(
    store: &BackgroundStore,
    state: &BackgroundJobState,
    ipc: &BackgroundIpcServer,
    session: &mut crate::EngineSession,
) -> anyhow::Result<WarmSessionWait> {
    let owner = ipc.owner();
    let job_id = state.identity.job_id.as_str();
    loop {
        let _ = tokio::time::timeout(Duration::from_secs(1), ipc.wake.notified()).await;
        if let Some(first) = ipc.try_recv_command() {
            run_idle_background_commands(store, state, ipc, session, first).await;
        }
        let current = store.read_state(job_id)?;
        if !owner.matches(&current) {
            tracing::info!(
                %job_id,
                recorded_pid = ?current.process.pid,
                "rebon: worker leaving linger — the job record names another owner"
            );
            return Ok(WarmSessionWait::Ended);
        }
        match current.process.status {
            BackgroundJobStatus::Queued if current.has_pending_prompts() => {
                let (cancel, claim) = ipc.start_turn_and(|| {
                    claim_background_job(
                        store,
                        job_id,
                        std::process::id(),
                        ipc.port,
                        &ipc.token,
                        process_is_running,
                    )
                });
                match claim? {
                    WorkerClaim::Claimed(claimed) => {
                        store.append_event(
                            job_id,
                            "worker_reused",
                            serde_json::json!({ "warmSession": true }),
                        )?;
                        return Ok(WarmSessionWait::Claimed { claimed, cancel });
                    }
                    WorkerClaim::NoWork => {}
                    WorkerClaim::Stopped | WorkerClaim::OwnedByOther(_) => {
                        return Ok(WarmSessionWait::Ended);
                    }
                }
            }
            BackgroundJobStatus::Stopped => {
                tracing::info!(%job_id, "rebon: worker leaving linger — the job was stopped");
                return Ok(WarmSessionWait::Ended);
            }
            BackgroundJobStatus::Idle
            | BackgroundJobStatus::Succeeded
            | BackgroundJobStatus::Failed => {}
            BackgroundJobStatus::Queued => {
                let normalized_at = now_ms();
                let still_owner = store.update_state(job_id, |current| {
                    if !owner.matches(current) {
                        return Ok(false);
                    }
                    if current.process.status == BackgroundJobStatus::Queued
                        && !current.has_pending_prompts()
                    {
                        current.process.status = BackgroundJobStatus::Idle;
                        current.process.updated_at_ms = normalized_at;
                    }
                    Ok(true)
                })?;
                if !still_owner {
                    return Ok(WarmSessionWait::Ended);
                }
            }
            // Only a claim moves the job here, and this worker is the only
            // one that claims under its pid — nothing this loop can follow.
            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput => {
                return Ok(WarmSessionWait::Ended);
            }
        }
        let idle_since = current
            .process
            .completed_at_ms
            .unwrap_or(current.process.updated_at_ms);
        if now_ms() >= current.linger_deadline_ms(idle_since) {
            tracing::info!(%job_id, "rebon: worker leaving linger — nobody asked for it in time");
            return Ok(WarmSessionWait::Ended);
        }
    }
}

pub(crate) async fn wait_for_terminal_worker_reuse(
    store: &BackgroundStore,
    job_id: &str,
    ipc: &BackgroundIpcServer,
    handed_back: &mut crate::session_handoff::HandedBack,
) -> anyhow::Result<bool> {
    let owner = ipc.owner();
    loop {
        // A tick, or sooner: a reply that came in over IPC rings `wake`, and
        // a prompt sent to a parked worker should not wait out the tick.
        let _ = tokio::time::timeout(Duration::from_secs(1), ipc.wake.notified()).await;
        let state = store.read_state(job_id)?;
        process_idle_background_commands(store, &state, ipc, handed_back).await;
        publish_held_mcp_status(store, &state, ipc, &mut handed_back.mcp).await;
        if !owner.matches(&state) {
            tracing::info!(
                %job_id,
                recorded_pid = ?state.process.pid,
                "rebon: worker leaving linger — the job record names another owner"
            );
            return Ok(false);
        }
        match state.process.status {
            BackgroundJobStatus::Queued if state.has_pending_prompts() => {
                store.append_event(job_id, "worker_reused", serde_json::json!({}))?;
                return Ok(true);
            }
            BackgroundJobStatus::Stopped => {
                tracing::info!(%job_id, "rebon: worker leaving linger — the job was stopped");
                return Ok(false);
            }
            BackgroundJobStatus::Idle
            | BackgroundJobStatus::Succeeded
            | BackgroundJobStatus::Failed => {}
            BackgroundJobStatus::Queued => {
                let normalized_at = now_ms();
                let still_owner = store.update_state(job_id, |current| {
                    if !owner.matches(current) {
                        return Ok(false);
                    }
                    if current.process.status == BackgroundJobStatus::Queued
                        && !current.has_pending_prompts()
                    {
                        current.process.status = BackgroundJobStatus::Idle;
                        current.process.updated_at_ms = normalized_at;
                    }
                    Ok(true)
                })?;
                if !still_owner {
                    return Ok(false);
                }
            }
            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput => {
                return Ok(true);
            }
        }
        let idle_since = state
            .process
            .completed_at_ms
            .unwrap_or(state.process.updated_at_ms);
        // A client holding a lease is watching this session, so the worker is
        // not idle in the sense that matters — it is standing by for someone.
        // The clock only starts once the last lease stops being renewed.
        if now_ms() >= state.linger_deadline_ms(idle_since) {
            tracing::info!(%job_id, "rebon: worker leaving linger — nobody asked for it in time");
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod retry_progress_tests {
    use super::*;

    fn runtime() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }

    fn store_with_job() -> (tempfile::TempDir, BackgroundStore, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        let job = store
            .create_job("hello".into(), std::path::PathBuf::from("."), runtime())
            .unwrap();
        (dir, store, job.identity.job_id)
    }

    fn published_retry(store: &BackgroundStore, job_id: &str) -> Option<BackgroundRetryProgress> {
        store.read_state(job_id).unwrap().outcome.retry
    }

    /// A turn that never retries must not pay for the poll: the state file is a
    /// file, and this runs every 500ms for the length of the turn.
    #[test]
    fn a_turn_that_never_retries_writes_nothing() {
        let (_dir, store, job_id) = store_with_job();
        let notifier = rebon_api::RetryNotifier::default();
        let mut published = None;

        let before = std::fs::metadata(store.job_dir(&job_id).join("state.json"))
            .unwrap()
            .modified()
            .unwrap();
        for _ in 0..5 {
            publish_retry_progress(&store, &job_id, &notifier, &mut published);
        }
        let after = std::fs::metadata(store.job_dir(&job_id).join("state.json"))
            .unwrap()
            .modified()
            .unwrap();

        assert_eq!(published, None);
        assert_eq!(
            before, after,
            "no write at all when there is nothing to say"
        );
    }

    #[test]
    fn a_retry_reaches_the_job_and_only_changes_are_written() {
        let (_dir, store, job_id) = store_with_job();
        let notifier = rebon_api::RetryNotifier::default();
        let mut published = None;

        notifier.set(2, 10);
        publish_retry_progress(&store, &job_id, &notifier, &mut published);
        assert_eq!(published_retry(&store, &job_id).map(|r| r.attempt), Some(2));

        // Same state, several polls later: the file must not be rewritten.
        let stamped = std::fs::metadata(store.job_dir(&job_id).join("state.json"))
            .unwrap()
            .modified()
            .unwrap();
        for _ in 0..5 {
            publish_retry_progress(&store, &job_id, &notifier, &mut published);
        }
        assert_eq!(
            std::fs::metadata(store.job_dir(&job_id).join("state.json"))
                .unwrap()
                .modified()
                .unwrap(),
            stamped,
            "an unchanged retry is not news"
        );

        notifier.set(3, 10);
        publish_retry_progress(&store, &job_id, &notifier, &mut published);
        let retry = published_retry(&store, &job_id).expect("advanced");
        assert_eq!(retry.to_string(), "Retry 3/10");
    }

    /// The indicator is cleared when the retrying stops — otherwise a turn that
    /// recovered goes on claiming it is retrying for as long as it runs.
    #[test]
    fn clearing_the_notifier_clears_the_job() {
        let (_dir, store, job_id) = store_with_job();
        let notifier = rebon_api::RetryNotifier::default();
        let mut published = None;

        notifier.set(1, 10);
        publish_retry_progress(&store, &job_id, &notifier, &mut published);
        assert!(published_retry(&store, &job_id).is_some());

        notifier.clear();
        publish_retry_progress(&store, &job_id, &notifier, &mut published);
        assert_eq!(published_retry(&store, &job_id), None);
        assert_eq!(published, None);
    }
}

/// Apply a `/ceo` command carried by this turn's prompt.
fn apply_background_ceo_command(session: &mut crate::EngineSession, prompt: &mut String) {
    // `/ceo` toggles coordinator (CEO) mode for this session, mirroring the
    // interactive TUI. `/ceo <task>` enters coordinator mode and runs the task;
    // bare `/ceo on` / `/ceo off` flip the mode. The mode is applied + persisted
    // *before* the turn, so it takes effect this turn and every subsequent per-turn
    // worker inherits it through the resume → `load_session_mode` path. The
    // toggle-only forms carry no task, so we run a one-line confirmation turn — the
    // background worker requires a durable transcript response to complete.
    if let Some(ceo) = crate::commands::ceo::parse_ceo_command(prompt.trim()) {
        use crate::commands::ceo::CeoCommand;
        // Keep the user's literal `/ceo …` text and steer the model with a reminder
        // — the same shape as the ultrawork opt-in. This matters for the
        // app's optimistic echo: the transcript projection strips the reminder
        // (`strip_injected_context`), leaving exactly the sent text, so the pending
        // send reconciles instead of leaving a duplicate bubble. Stripping the
        // `/ceo` prefix here (persisting only the bare task) would break that match.
        let reminder = match ceo {
            CeoCommand::Task(_) => {
                session.apply_coordinator_mode(true);
                "用户通过 /ceo 开启了 CEO / 协调器模式（现已对本会话生效），并在命令后附带了任务。请以协调器身份拆解并推进 /ceo 之后的任务。"
            }
            CeoCommand::On => {
                session.apply_coordinator_mode(true);
                "用户通过 /ceo 开启了 CEO / 协调器模式，现已对本会话生效。请用一句话确认已进入协调器模式，然后等待用户的下一个任务；本轮不要自行展开工作。"
            }
            CeoCommand::Off => {
                session.apply_coordinator_mode(false);
                "用户通过 /ceo 关闭了 CEO / 协调器模式，现已回到普通模式。请用一句话确认已退出协调器模式，然后等待用户的下一个任务；本轮不要自行展开工作。"
            }
        };
        *prompt = format!("<system-reminder>\n{reminder}\n</system-reminder>\n\n{prompt}");
    }
}
