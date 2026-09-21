use super::super::*;

/// What a worker hands the session-control commands instead of a screen.
///
/// A terminal fills `SessionCommandInputs` off its `AppState`; a worker has
/// no screen, so the rows a command would count are replayed here from the
/// loaded transcript, and everything else is read straight off the engine
/// half. The terminal-only fields stay empty, which is the fact rather than
/// a gap: a worker has no vim mode, no ultraplan widget, no update notice.
pub struct WorkerCommandInputs {
    rows: Vec<rebon_render::transcript_row::Message>,
    ui_mode: crate::ui_config::UiMode,
}

impl WorkerCommandInputs {
    pub fn from_session(session: &crate::EngineSession, ui_mode: crate::ui_config::UiMode) -> Self {
        // The replay also reports the async Agent launches the transcript
        // recorded. Only a screen has a use for those, joining them against
        // live task snapshots to draw activity, and a worker has no screen.
        let rows = if session.loaded_transcript.is_empty() {
            Vec::new()
        } else {
            let (rows, _agent_launches) =
                crate::transcript_replay::replayed_messages(session.loaded_transcript.clone());
            rows
        };
        Self { rows, ui_mode }
    }

    /// The inputs a session-control command reads, borrowed for one call.
    pub fn inputs<'a>(
        &'a self,
        session: &'a crate::EngineSession,
    ) -> crate::commands::SessionCommandInputs<'a> {
        crate::commands::SessionCommandInputs {
            rows: &self.rows,
            permission_mode: *session
                .engine_half
                .permission_mode_cell
                .lock()
                .expect("mode cell poisoned"),
            usage: session
                .engine_half
                .usage_ledger
                .lock()
                .expect("usage ledger poisoned")
                .snapshot(),
            streaming_token_count: 0,
            auto_mode_denials: &session.engine_half.auto_mode_denials,
            auto_mode_verdicts: &session.engine_half.auto_mode_verdicts,
            task_snapshots: session.engine_half.tasks.snapshots(),
            session_title: None,
            ui_mode: self.ui_mode,
            vim_mode: None,
            ultraplan_phase: None,
            update_notice: None,
        }
    }
}

/// The one command both control surfaces refuse mid-turn.
///
/// `/permissions retry` replays denied tool calls in a *new* turn. Started while
/// a turn is still in flight, that replay is queued into a supplemental turn the
/// running one may no longer own, so the retried calls land against a turn the
/// user never asked for. Returns the refusal to hand back, or `None` when the
/// command is not a retry. Shared so the foreground mailbox and the background
/// IPC server answer identically.
pub fn permission_retry_blocked_by_running_turn(
    name: &str,
    args: &[String],
) -> Option<rebon_session_host::CommandOutput> {
    let is_permission_retry = name
        .trim()
        .trim_start_matches('/')
        .eq_ignore_ascii_case("permissions")
        && args.first().is_some_and(|arg| arg == "retry");
    is_permission_retry.then(|| rebon_session_host::CommandOutput {
        text: "Cannot retry permissions while a prompt is running. Please retry after the current turn finishes.".into(),
        tone: "warning".into(),
    })
}

/// When a command drain is happening, relative to the turn it arrived during.
///
/// A command means different things at each point: only an idle worker can hold
/// a context command over for its next turn, and only a running turn has to
/// refuse a permission retry.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandDrainPhase {
    /// A turn is in flight; the drain is racing it.
    DuringTurn,
    /// The turn's execution future has resolved, before its replay runs.
    AfterTurn,
    /// Between turns, on a worker parked for reuse.
    Idle,
}

/// Whether a slash command names `/compact`.
///
/// One copy, read by both control planes: the worker's IPC path here, and the
/// foreground mailbox in the binary. It was written twice, byte for byte,
/// until a later pass found them.
pub fn is_compact_command(name: &str) -> bool {
    name.trim()
        .trim_start_matches('/')
        .eq_ignore_ascii_case("compact")
}

/// Compact an idle job's history now, and answer with what it kept.
///
/// The desktop app's `/compact` lands here. It blocks the drain for the
/// length of the provider call on purpose: the worker is parked between
/// turns with nothing else to do, and returning early would let the process
/// exit — or the app's next prompt start — before the compaction it is
/// waiting on has landed. `run_background_command` allows for the wait.
///
/// Returns `Err` when there is no runtime to drive the call from, which is
/// the caller's cue to fall back to arming the next request instead.
fn compact_idle_background_job_now(
    session: &crate::EngineSession,
    args: &[String],
) -> Result<rebon_session_host::CommandOutput, String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "no async runtime is available to run the compaction".to_string())?;
    let path = rebon_session::transcript_file_path(
        &session.projects_root,
        &session.cwd,
        &session.session_id,
    );
    let loaded = rebon_session::load_transcript_from_file(&path)
        .map_err(|err| format!("Could not read this session's transcript: {err}"))?
        .ok_or_else(|| "This session has no transcript to compact yet.".to_string())?;

    let instructions = (!args.is_empty()).then(|| args.join(" "));
    let resume_replay = session.engine_half.resume_replay();
    let entries = loaded.messages;
    // A dedicated OS thread drives the future: this function is called from
    // inside the runtime, so `block_on` here would panic, and the drain has
    // to stay put until the answer exists.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = handle.block_on(resume_replay.compact_now(&entries, instructions.as_deref()));
        let _ = tx.send(result);
    });
    let (prepared, report) = rx
        .recv()
        .map_err(|_| "Compaction stopped before returning a result.".to_string())??;

    session
        .engine_half
        .resume_replay()
        .install_persistent_summary(
            &session.projects_root,
            &session.cwd,
            session.session_id.clone(),
            prepared,
        );
    session
        .model
        .prune_level
        .report_estimated_usage(report.tokens_after);
    session.model.prune_level.budget.record_compact_success();
    Ok(rebon_session_host::CommandOutput {
        text: crate::compact::render_compact_report(&report),
        tone: "info".into(),
    })
}

pub(crate) fn execute_background_command_request(
    ipc: &BackgroundIpcServer,
    command_inputs: &WorkerCommandInputs,
    session: &crate::EngineSession,
    request: BackgroundCommandRequest,
    phase: CommandDrainPhase,
) -> Vec<rebon_agent_core::DenialReplayRequest> {
    if phase == CommandDrainPhase::DuringTurn {
        if let Some(output) = permission_retry_blocked_by_running_turn(&request.name, &request.args)
        {
            let _ = request.response_tx.send(Ok(output));
            return Vec::new();
        }
    }

    // `/compact` on a parked worker runs for real. Mid-turn it stays a flag
    // on the next request, because the running turn owns the live context
    // manager and is about to compact inside it anyway.
    if phase == CommandDrainPhase::Idle && is_compact_command(&request.name) {
        match compact_idle_background_job_now(session, &request.args) {
            Ok(output) => {
                let _ = request.response_tx.send(Ok(output));
                return Vec::new();
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "background /compact could not run immediately; arming the next request instead"
                );
            }
        }
    }
    let inputs = command_inputs.inputs(session);
    let result = crate::commands::control::execute_session_control_command(
        &inputs,
        session,
        &request.name,
        &request.args,
    );
    if result.is_ok() {
        if phase == CommandDrainPhase::Idle {
            ipc.capture_idle_context_command(session, &request.name, &request.args);
        } else if request
            .name
            .trim()
            .trim_start_matches('/')
            .eq_ignore_ascii_case("prune")
            && request.args.first().is_some_and(|arg| arg == "manual")
        {
            *ipc.persistent_prune_level.lock().expect("poisoned") =
                Some(session.model.prune_level.get());
        }
    }
    let mut replay_requests = Vec::new();
    let response = result
        .map(|result| {
            replay_requests = result.replay_requests;
            result.output
        })
        .or_else(super::server::request_refused);
    let _ = request.response_tx.send(response);
    replay_requests
}

pub(crate) fn drain_background_commands(
    ipc: &BackgroundIpcServer,
    command_inputs: &WorkerCommandInputs,
    session: &crate::EngineSession,
    phase: CommandDrainPhase,
) -> Vec<rebon_agent_core::DenialReplayRequest> {
    let mut replay_requests = Vec::new();
    while let Some(request) = ipc.try_recv_command() {
        replay_requests.extend(execute_background_command_request(
            ipc,
            command_inputs,
            session,
            request,
            phase,
        ));
    }
    replay_requests
}

pub(crate) async fn execute_background_permission_replay(
    session: &crate::EngineSession,
    replay_requests: Vec<rebon_agent_core::DenialReplayRequest>,
) -> Result<(), String> {
    if replay_requests.is_empty() {
        return Ok(());
    }
    let request = rebon_agent_core::PromptRequest {
        user_prompt: None,
        effort_is_session_default: false,
        session_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        prompt: Vec::new(),
        update_publisher: Some(session.engine_half.update_publisher.clone()),
        permission_publisher: None,
        mcp_servers: Vec::new(),
        cancel: rebon_types::PromptCancel::new(),
        thinking_budget: None,
        max_tokens: None,
        reasoning_effort_ordinal: None,
        additional_working_directories: session.startup.add_dirs.clone(),
        coordinator_mode: Some(session.engine_half.coordinator_mode_handle.get()),
        coordinator_report_paths: Vec::new(),
        user_message_uuid: None,
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests,
        skill_invocations: Vec::new(),
    };
    session
        .engine_half
        .executor
        .execute(request)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(crate) fn queue_live_background_reply(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    message: String,
    images: Vec<BackgroundImageAttachment>,
) -> anyhow::Result<()> {
    let queued_at = now_ms();
    let prompt = PendingPrompt::new(generate_pending_prompt_id(), message, images, queued_at)?;
    queue_live_background_prompt(store, job_id, owner, prompt)
}

/// The queue mutation is shared by delivery acknowledgements and ACP turns.
/// The latter registers its reply sink under this prompt's id before waking
/// the worker, so a fast claim/completion cannot outrun registration.
pub(crate) fn queue_live_background_prompt(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    prompt: PendingPrompt,
) -> anyhow::Result<()> {
    let queued_at = now_ms();
    store.update_state(job_id, |state| {
        owner.ensure_matches(state)?;
        let acceptance = state.append_pending_prompt(prompt.clone())?;
        if !acceptance.appended {
            return Ok(());
        }
        if !matches!(
            state.process.status,
            BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
        ) {
            state.process.status = BackgroundJobStatus::Queued;
            state.outcome.pending_permission = None;
            state.outcome.summary = None;
            state.process.completed_at_ms = None;
            state.outcome.exit_code = None;
            state.outcome.error = None;
        }
        state.identity.resume_only = false;
        state.outcome.summary_updated_at_ms = None;
        state.process.updated_at_ms = queued_at;
        Ok(())
    })?;
    Ok(())
}
