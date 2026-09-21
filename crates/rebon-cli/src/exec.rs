//! `rebon exec` — headless one-shot prompt runner with a JSONL event feed.
//!
//! Builds an in-process engine session via [`rebon_harness::build_headless_session`]
//! (the same path the GPUI app uses), runs a single agentic turn to completion,
//! and projects the raw [`QueryEvent`] stream onto stdout — one JSON object per
//! line in `--json` mode, or a compact human trace otherwise.
//!
//! Design notes:
//!
//! * **stdout is the event stream.** Tracing is routed to stderr by the caller
//!   (`init_tracing(false)`), so machine consumers get a clean feed.
//! * **Exact tool names.** We observe `QueryEvent`s directly (via
//!   [`QueryEventObserver`]) rather than the ACP `session/update` projection,
//!   because the latter flattens the registered tool name into a human title.
//!   `action.called` therefore carries the real name (`Read`, `get_weather`, …).
//! * **Unattended permissions.** Eval runs have nobody to click "allow", so a
//!   background task drains the permission channel and auto-approves every
//!   request (bypass mode). Mirrors the app's `answer_permission` path.
//! * **Session continuity.** The session id is printed on the first line; pass
//!   it back via `--resume <id>` to continue the conversation across turns.

use anyhow::Context;
use rebon_agent_core::turn_gate::{TurnCompletionGate, TurnGatePolicy};
use rebon_agent_core::{PromptCancel, PromptRequest};
use rebon_api::ContentBlock as ApiContentBlock;
use rebon_command_seat::{resolve_typed_line_on_process_seat, Surface, TypedLine};
use rebon_config::ProviderFormat;
use rebon_core::permission::{PermissionAnswer, PermissionOptionKind, PermissionQueryOption};
use rebon_core::query::{QueryEvent, QueryEventObserver};
use rebon_harness::{build_headless_session, HarnessOverrides, HeadlessSession};
use rebon_permissions::PermissionMode;
use rebon_types::{effort_indicator::EffortProviderKind, ReasoningEffort};
use rebon_types::{AgentCapabilityMode, ContentBlock as AcpContentBlock, TextContent};

use crate::session::commands::effort::{resolve_thinking_from_effort, ThinkingOverrides};

use std::path::PathBuf;
use std::time::Duration;

/// Parsed inputs for `rebon exec`, assembled from the subcommand args plus the
/// global `--provider` / `--model` / `--effort` flags.
pub struct ExecArgs {
    pub prompt: String,
    pub json: bool,
    pub resume: Option<String>,
    pub max_iterations: Option<usize>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<ReasoningEffort>,
    pub permission_mode: Option<PermissionMode>,
    pub capability_mode: AgentCapabilityMode,
    pub plugin_dirs: Vec<PathBuf>,
    /// Audit rounds to allow after the model ends its own turn. `0` is off,
    /// and off is the default: an interactive caller who says "done" and means
    /// it should not have to argue with the harness. Unattended callers — a
    /// benchmark, a cron agent, anything with hours of budget and nobody
    /// watching — are the ones who want this. See [`TurnGatePolicy`].
    pub verify_rounds: u32,
    /// Wall-clock ceiling for opening new audit rounds, in seconds.
    pub verify_budget_sec: Option<u64>,
    /// Hard wall-clock ceiling for the whole run, in seconds.
    ///
    /// Distinct from [`Self::verify_budget_sec`], which only stops *new* audit
    /// rounds from opening: this one cancels whatever is running. Without it an
    /// unattended turn is bounded by nothing but `max_iterations`, and 600
    /// iterations is not a duration — measured on Terminal-Bench 4.0, single
    /// trials ran 4.8 and 6.9 hours and scored zero, while the trials that
    /// scored anything overwhelmingly finished inside two.
    ///
    /// It bounds the *agent loop*, not a shell command already running. A tool
    /// in flight is interrupted and its result reported as such, but its child
    /// process is left to its own timeout (Bash and PowerShell: 60 s by
    /// default, 600 s at most), and the process exits after that. So the real
    /// ceiling is this value plus at most one tool timeout.
    pub max_duration_sec: Option<u64>,
}

fn resolve_exec_thinking(
    effort: Option<ReasoningEffort>,
    provider_format: ProviderFormat,
) -> Option<ThinkingOverrides> {
    let provider_kind = match provider_format {
        ProviderFormat::Anthropic => EffortProviderKind::Anthropic,
        ProviderFormat::Openai | ProviderFormat::OpenaiResponses => EffortProviderKind::OpenAi,
    };
    effort.map(|level| resolve_thinking_from_effort(Some(level), provider_kind))
}

fn select_unattended_allow_option(options: &[PermissionQueryOption]) -> Option<String> {
    let preferred = options.iter().find(|option| {
        option.option_id == "yes_auto"
            && matches!(
                option.kind,
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
            )
    });
    preferred
        .or_else(|| {
            options.iter().find(|option| {
                option.option_id != "yes_clear_context_auto"
                    && matches!(
                        option.kind,
                        PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                    )
            })
        })
        .map(|option| option.option_id.clone())
}

/// Run one headless turn and stream its events to stdout.
pub async fn run(args: ExecArgs) -> anyhow::Result<()> {
    if args.prompt.trim().is_empty() {
        anyhow::bail!("rebon exec: empty prompt");
    }

    // Say out loud what this process is, before anything builds a tool list
    // from it. `exec` has no one to approve a plan, answer a question, or
    // retry a call the classifier stopped, and the tools that need one of
    // those are hidden rather than offered and refused.
    // `REBON_EXECUTION_SURFACE=interactive` opts back out for a run someone
    // is in fact babysitting.
    rebon_tool::set_execution_surface(rebon_tool::ExecutionSurface::Unattended);

    let json = args.json;
    let gate_policy = {
        let mut policy = TurnGatePolicy::with_rounds(args.verify_rounds);
        if let Some(seconds) = args.verify_budget_sec.filter(|s| *s > 0) {
            policy = policy.budget(std::time::Duration::from_secs(seconds));
        }
        policy
    };

    // Observe every raw QueryEvent and project it onto stdout. The callback runs
    // synchronously inside the executor's single consume loop, so lines are
    // emitted in event order without interleaving.
    let observer = QueryEventObserver::new(move |event| {
        if json {
            for value in project_json(event) {
                emit(&value);
            }
        } else {
            project_text(event);
        }
    });

    let plugin_runtime =
        crate::plugin::resolve_runtime_contributions(&crate::plugin::PluginRuntimeOptions {
            cwd: std::env::current_dir()
                .context("failed to read the current directory for rebon exec")?,
            config_home: crate::rebon_config::config_home_dir(),
            plugin_dirs: args.plugin_dirs,
            rebon_exe: std::env::current_exe().ok(),
        })?;
    for warning in &plugin_runtime.warnings {
        tracing::warn!("rebon exec: plugin runtime warning: {warning}");
    }

    let overrides = HarnessOverrides {
        provider: args.provider,
        model: args.model,
        // `rebon exec` has no `--fast`; read the saved setting as before.
        fast_mode: None,
        cwd: None,
        resume_session_id: args.resume,
        max_iterations: args.max_iterations,
        permission_mode: args.permission_mode,
        capability_mode: args.capability_mode,
        coordinator_mode: false,
        // The user's setting, not a hardcoded no. It reads `true` unless
        // someone turned sub-agents off, which is how every other surface
        // reads it; exec used to override that silently because it had no
        // spawner to honour it with (now wired in `build_headless_session`).
        sub_agents_enabled: crate::rebon_config::saved_sub_agents_enabled(),
        plugin_model_providers: plugin_runtime.model_providers,
        // Plugin-contributed hooks join the user's `settings.json` hooks on
        // the session's policy handle. Resolved here because reading
        // `--plugin-dirs` and expanding a manifest's placeholders is this
        // binary's job, not the harness's.
        plugin_hooks: plugin_runtime.hooks,
        query_event_observer: Some(observer),
    };

    let HeadlessSession {
        executor,
        client,
        mut permission_rx,
        session_id,
        cwd,
        model,
        // Held for the session lifetime — dropping these early would let a
        // resume collide (the state owns the session's active lock) or sever
        // the update/permission plumbing.
        update_publisher: _update_publisher,
        permission_broker: _permission_broker,
        server_state: _server_state,
        ..
    } = build_headless_session(overrides)
        .await
        .context("failed to build headless session (check provider/model config)")?;

    // The turn-completion gate, wrapped here rather than inside the harness so
    // it stays a property of *this* invocation: `exec` is the unattended
    // surface, and unattended is exactly where "I'm done" goes unchallenged
    // because there is nobody to challenge it. `wrap_if_enabled` returns the
    // executor untouched when rounds are 0, so the default path is unchanged.
    let executor = TurnCompletionGate::wrap_if_enabled(executor, gate_policy);

    // Announce the session id first so the caller can capture it for --resume.
    // `anchoredMinimal` reports whether the Anchored Minimal bootstrap is
    // actually engaged: asking for `--capability minimal` is not enough, the
    // provider has to opt in too, and otherwise Minimal quietly degrades to a
    // plain reduced-tool request — which a sampling harness must not mistake
    // for an anchored run.
    let anchored_minimal = args.capability_mode.is_minimal() && client.supports_anchored_minimal();
    if json {
        emit(&serde_json::json!({
            "type": "session",
            "sessionId": session_id,
            "capabilityMode": args.capability_mode,
            "anchoredMinimal": anchored_minimal,
        }));
    } else {
        eprintln!(
            "session {session_id} · capability {:?} · anchored_minimal {anchored_minimal}",
            args.capability_mode
        );
    }

    // Unattended auto-approver: answer every permission request with a
    // non-destructive allow option. ExitPlanMode prefers `yes_auto` and never
    // selects the legacy clear-context option, because unattended runs must
    // retain the original user request.
    let approver = tokio::spawn(async move {
        while let Some(query) = permission_rx.recv().await {
            let answer = match select_unattended_allow_option(&query.options) {
                Some(option_id) => PermissionAnswer::Selected {
                    option_id,
                    updated_input: None,
                    extra_text: None,
                },
                // No safe allow option offered → cancel so the tool future resolves.
                None => PermissionAnswer::Cancelled,
            };
            let _ = query.response_tx.send(answer);
        }
    });

    let thinking_overrides = resolve_exec_thinking(args.effort, model.provider_format);
    let (thinking_budget, max_tokens, reasoning_effort_ordinal) = match thinking_overrides {
        Some(overrides) => (
            overrides.thinking_budget,
            overrides.max_tokens,
            overrides.reasoning_effort_ordinal,
        ),
        None => (None, None, None),
    };

    // Slash commands, by the handler the `command-registry` seat carries
    // (RFC kernel-plugins §13). A prompt-shaped command — a plugin's, or a
    // skill's — expands into this turn the way a terminal expands it; before
    // this, `exec` sent `/name args` to the model with the slash still on it
    // and asked it to guess. Resolved here rather than at the top of the
    // function because the seat exists once the session has booted the kernel.
    let prompt_text = match resolve_exec_prompt(args.prompt).await {
        ExecPrompt::Send(text) => text,
        ExecPrompt::Answered(text) => {
            approver.abort();
            _server_state.close_session(&session_id);
            if json {
                emit(&serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "text": text,
                }));
                emit(&serde_json::json!({
                    "type": "result",
                    "sessionId": session_id,
                    "stopReason": "command",
                    "usage": serde_json::Value::Null,
                }));
            } else {
                println!("{text}");
                eprintln!("done · stop=command");
            }
            return Ok(());
        }
    };

    let request = PromptRequest {
        user_prompt: Some(prompt_text.clone()),
        effort_is_session_default: true,
        session_id: session_id.clone(),
        cwd,
        prompt: vec![AcpContentBlock::Text(TextContent {
            text: prompt_text,
            annotations: None,
        })],
        mcp_servers: Vec::new(),
        // Output is driven by the QueryEvent observer, not the ACP update sink.
        update_publisher: None,
        permission_publisher: None,
        cancel: PromptCancel::default(),
        thinking_budget,
        max_tokens,
        reasoning_effort_ordinal,
        additional_working_directories: Vec::new(),
        coordinator_mode: Some(false),
        coordinator_report_paths: Vec::new(),
        user_message_uuid: None,
        background_agent_system: None,
        background_agent_tool_filter: None,
        execution_policy: None,
        replay_requests: Vec::new(),
        skill_invocations: Vec::new(),
    };

    // The ceiling cancels through the same handle the caller would: the engine
    // already unwinds a cancel cleanly (in-flight tools interrupted, transcript
    // written), so the turn is awaited to completion afterwards rather than
    // dropped mid-flight. A dropped future would leave the session's last turn
    // unrecorded, and `--resume` reads exactly that.
    let deadline = args.max_duration_sec.map(Duration::from_secs);
    let cancel = request.cancel.clone();
    let mut deadline_reached = false;
    let result = match deadline {
        Some(limit) => {
            let mut turn = std::pin::pin!(executor.execute(request));
            tokio::select! {
                outcome = &mut turn => outcome,
                () = tokio::time::sleep(limit) => {
                    deadline_reached = true;
                    tracing::warn!(
                        max_duration_sec = limit.as_secs(),
                        "rebon exec: wall-clock ceiling reached, cancelling the turn"
                    );
                    cancel.cancel();
                    turn.await
                }
            }
        }
        None => executor.execute(request).await,
    };

    // Tear down the approver once the turn is over, and give the session up
    // before the result is printed so a resume does not have to wait on stdout.
    approver.abort();
    _server_state.close_session(&session_id);

    match result {
        Ok(outcome) => {
            // A caller cannot tell "the harness stopped this" from "the user
            // pressed Ctrl-C" out of `cancelled` alone, and the two mean
            // opposite things to whatever reads the run afterwards.
            let stop_reason = if deadline_reached {
                serde_json::Value::String("max_duration".into())
            } else {
                serde_json::to_value(outcome.stop_reason).unwrap_or(serde_json::Value::Null)
            };
            if json {
                emit(&serde_json::json!({
                    "type": "result",
                    "sessionId": session_id,
                    "stopReason": stop_reason,
                    "usage": outcome.usage,
                }));
            } else {
                eprintln!(
                    "done · stop={} · in={} out={}",
                    stop_reason, outcome.usage.input_tokens, outcome.usage.output_tokens
                );
            }
            Ok(())
        }
        // The ceiling cancels the turn, so the executor reports a cancel. That
        // is this process doing what it was asked, not a failure — exiting
        // non-zero here would tell every harness that reads the exit code that
        // the run crashed, and the ones that retry on a crash would throw away
        // exactly the work the ceiling was protecting.
        Err(rebon_agent_core::PromptExecutorError::Cancelled) if deadline_reached => {
            if json {
                emit(&serde_json::json!({
                    "type": "result",
                    "sessionId": session_id,
                    "stopReason": "max_duration",
                    // Not carried on the cancel path; the per-turn
                    // `turn.completed` records still have it.
                    "usage": serde_json::Value::Null,
                }));
            } else {
                eprintln!("done · stop=max_duration");
            }
            Ok(())
        }
        Err(err) => {
            let message = format!("{err:?}");
            if json {
                emit(&serde_json::json!({ "type": "error", "message": message }));
            }
            Err(anyhow::anyhow!("rebon exec turn failed: {message}"))
        }
    }
}

/// What a typed slash command turns `rebon exec`'s prompt into.
#[derive(Debug, PartialEq, Eq)]
enum ExecPrompt {
    /// Ask the model this.
    Send(String),
    /// Say this and finish, with no turn at all.
    Answered(String),
}

/// Resolve `rebon exec`'s prompt against the `command-registry` seat.
///
/// `exec` keeps no table of native commands — there is no screen to open a
/// panel on and nobody to answer a question — so only a `Prompt` handler
/// changes what the model is asked, and `Explain` / `Panel` end the run with a
/// sentence instead. A `Native` id and a name nothing registered both go to
/// the model exactly as typed: that is what leaves a user skill's `/name` for
/// the engine to resolve (`resolve_typed_skill_invocation`), which is how
/// skills already worked here.
///
/// Blocking on a thread of its own: a plugin's handler round-trips to the
/// plugin host process. The kernel must already be booted, so the caller runs
/// this after the session is built.
///
/// The surface a handler is told about is [`Surface::Acp`]: what that name
/// means to a handler is "headless — no panel to open, nobody to ask", which
/// is exactly what `exec` is. A sixth surface would need a bit in `Surfaces`
/// and a spelling on the plugin wire, and no command yet wants to tell the
/// two apart.
async fn resolve_exec_prompt(prompt: String) -> ExecPrompt {
    let resolved = tokio::task::spawn_blocking(move || {
        let line = prompt.trim_end().to_string();
        (
            resolve_typed_line_on_process_seat(&line, Surface::Acp),
            prompt,
        )
    })
    .await;
    let (resolved, prompt) = match resolved {
        Ok(pair) => pair,
        Err(error) => {
            tracing::error!(%error, "a slash command handler did not return");
            return ExecPrompt::Answered("That command could not be run.".to_string());
        }
    };
    match resolved {
        TypedLine::Expanded(expanded) => ExecPrompt::Send(expanded),
        TypedLine::Say(text) => ExecPrompt::Answered(text),
        TypedLine::Panel(dialog) => ExecPrompt::Answered(format!(
            "That command opens the `{dialog}` panel, which `rebon exec` has no surface for."
        )),
        TypedLine::NotACommand | TypedLine::Native { .. } => ExecPrompt::Send(prompt),
    }
}

/// Print one JSON value as a single stdout line.
fn emit(value: &serde_json::Value) {
    if let Ok(line) = serde_json::to_string(value) {
        println!("{line}");
    }
}

/// Project one raw [`QueryEvent`] into zero or more machine-readable JSON event
/// objects (the `--json` stream vocabulary). Pure — no I/O — so it can be
/// unit-tested. `IterationComplete` fans out to one object per text / thinking
/// block; most events map to a single object; some map to none.
///
/// The terminal `Done` is intentionally NOT projected here: its usage and stop
/// reason are printed authoritatively from the `execute()` return value so the
/// `result` line always comes last.
fn project_json(event: &QueryEvent) -> Vec<serde_json::Value> {
    match event {
        QueryEvent::ToolDispatchStart {
            tool_use_id,
            name,
            input,
        } => vec![serde_json::json!({
            "type": "action.called",
            "callId": tool_use_id,
            "name": name,
            "input": input,
        })],
        QueryEvent::ToolDispatchResult {
            tool_use_id,
            name,
            outcome,
            error_presentation,
        } => {
            let (status, output) = match outcome {
                Ok(value) => ("completed", value.clone()),
                Err(err) => (
                    "failed",
                    serde_json::Value::String(
                        error_presentation
                            .as_ref()
                            .map(|presentation| presentation.display_message.clone())
                            .unwrap_or_else(|| err.clone()),
                    ),
                ),
            };
            vec![serde_json::json!({
                "type": "action.result",
                "callId": tool_use_id,
                "name": name,
                "status": status,
                "output": output,
            })]
        }
        QueryEvent::IterationComplete { iteration, message } => {
            let mut events: Vec<_> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ApiContentBlock::Text(text) if !text.text.trim().is_empty() => {
                        Some(serde_json::json!({
                            "type": "message",
                            "iteration": iteration,
                            "role": "assistant",
                            "text": text.text,
                        }))
                    }
                    ApiContentBlock::Thinking(thinking) if !thinking.thinking.trim().is_empty() => {
                        Some(serde_json::json!({
                            "type": "thinking",
                            "iteration": iteration,
                            "text": thinking.thinking,
                        }))
                    }
                    _ => None,
                })
                .collect();
            events.push(serde_json::json!({
                "type": "turn.completed",
                "iteration": iteration,
                "model": message.model,
                "stopReason": message.stop_reason,
                "usage": message.usage,
            }));
            events
        }
        QueryEvent::CompactingStarted { .. } => {
            vec![serde_json::json!({ "type": "compaction", "phase": "started" })]
        }
        QueryEvent::CompactingFinished { .. } => {
            vec![serde_json::json!({ "type": "compaction", "phase": "finished" })]
        }
        QueryEvent::IterationLimitReached { iterations } => vec![serde_json::json!({
            "type": "error",
            "message": format!("iteration limit reached after {iterations} iterations"),
        })],
        QueryEvent::Error(message) => {
            vec![serde_json::json!({ "type": "error", "message": message })]
        }
        QueryEvent::Cancelled => {
            vec![serde_json::json!({ "type": "error", "message": "cancelled" })]
        }
        // Streaming deltas, tool progress, attachment injections, context resets,
        // permission queries and the terminal Done are not projected.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn permission_option(option_id: &str, kind: PermissionOptionKind) -> PermissionQueryOption {
        PermissionQueryOption {
            option_id: option_id.into(),
            label: option_id.into(),
            kind,
        }
    }

    /// A prompt-shaped command expands into the turn `exec` runs.
    ///
    /// Before this, `exec` was the one surface with no command layer at all:
    /// `/name args` went to the model with the slash still on it. The three
    /// cases are the whole contract — a handler that answers becomes the
    /// prompt, one that fails ends the run with a sentence, and a name
    /// nothing registered goes to the model as typed, which is what leaves a
    /// user skill's `/name` for the engine to resolve.
    ///
    /// Registered on this binary's one process seat, the way the sibling
    /// tests in `session::commands` do; the names are unique to this test so
    /// a parallel sibling reading the seat does not see them as its own.
    #[tokio::test]
    async fn a_prompt_command_expands_before_the_turn() {
        let kernel = rebon_harness::kernel_bootstrap::process_kernel();
        let seat = rebon_kernel_seats::kernel_core_commands::command_seat()
            .expect("core-commands is a Core plugin and always loads");
        let plugin = kernel.context().fork("test-plugin-exec-command");
        seat.register(
            &plugin,
            rebon_slash_commands::CommandSpec::new("exec-fixture", "A fixture"),
            rebon_kernel_seats::kernel_core_commands::CommandHandler::Prompt(std::sync::Arc::new(
                |args: &rebon_kernel_seats::kernel_core_commands::CommandArgs| {
                    Ok(format!("Review {} carefully.", args.rest))
                },
            )),
        )
        .expect("the name is free");
        seat.register(
            &plugin,
            rebon_slash_commands::CommandSpec::new("exec-fixture-silent", "Never answers"),
            rebon_kernel_seats::kernel_core_commands::CommandHandler::Prompt(std::sync::Arc::new(
                |_: &rebon_kernel_seats::kernel_core_commands::CommandArgs| {
                    Err("the plugin never answered".to_string())
                },
            )),
        )
        .expect("the name is free");

        assert_eq!(
            resolve_exec_prompt("/exec-fixture the diff".to_string()).await,
            ExecPrompt::Send("Review the diff carefully.".to_string())
        );
        assert_eq!(
            resolve_exec_prompt("/exec-fixture-silent".to_string()).await,
            ExecPrompt::Answered("the plugin never answered".to_string())
        );
        // A built-in the terminal implements has no meaning here, and an
        // unregistered name is a skill or plain prose: both go as typed.
        assert_eq!(
            resolve_exec_prompt("/vim".to_string()).await,
            ExecPrompt::Send("/vim".to_string())
        );
        assert_eq!(
            resolve_exec_prompt("/release 1.2.0".to_string()).await,
            ExecPrompt::Send("/release 1.2.0".to_string())
        );
        assert_eq!(
            resolve_exec_prompt("ordinary prose".to_string()).await,
            ExecPrompt::Send("ordinary prose".to_string())
        );

        plugin.dispose();
        assert_eq!(
            resolve_exec_prompt("/exec-fixture the diff".to_string()).await,
            ExecPrompt::Send("/exec-fixture the diff".to_string()),
            "the command left with its plugin"
        );
    }

    #[test]
    fn unattended_approval_prefers_yes_auto_over_clear_context() {
        let options = vec![
            permission_option("yes_clear_context_auto", PermissionOptionKind::AllowOnce),
            permission_option("yes_auto", PermissionOptionKind::AllowOnce),
            permission_option("yes_default", PermissionOptionKind::AllowOnce),
        ];

        assert_eq!(
            select_unattended_allow_option(&options).as_deref(),
            Some("yes_auto")
        );
    }

    #[test]
    fn unattended_approval_rejects_clear_context_only_request() {
        let options = vec![permission_option(
            "yes_clear_context_auto",
            PermissionOptionKind::AllowOnce,
        )];

        assert_eq!(select_unattended_allow_option(&options), None);
    }

    #[test]
    fn unattended_approval_keeps_first_regular_allow_option() {
        let options = vec![
            permission_option("reject_once", PermissionOptionKind::RejectOnce),
            permission_option("allow_once", PermissionOptionKind::AllowOnce),
            permission_option("allow_always", PermissionOptionKind::AllowAlways),
        ];

        assert_eq!(
            select_unattended_allow_option(&options).as_deref(),
            Some("allow_once")
        );
    }

    #[test]
    fn omitted_effort_preserves_provider_defaults() {
        assert!(resolve_exec_thinking(None, ProviderFormat::Openai).is_none());
        assert!(resolve_exec_thinking(None, ProviderFormat::Anthropic).is_none());
    }

    #[test]
    fn openai_effort_levels_map_to_reasoning_ordinals() {
        // `max_tokens` has to climb with effort here: on these providers
        // it also covers the hidden reasoning, so a flat cap lets a
        // high-effort chain of thought consume the whole budget and end
        // the turn with nothing visible emitted.
        for (level, ordinal, max_tokens) in [
            (ReasoningEffort::Low, 0, 16_000),
            (ReasoningEffort::Medium, 1, 32_000),
            (ReasoningEffort::High, 2, 64_000),
            (ReasoningEffort::XHigh, 3, 128_000),
            (ReasoningEffort::Max, 4, 128_000),
        ] {
            let overrides = resolve_exec_thinking(Some(level), ProviderFormat::Openai).unwrap();
            assert_eq!(overrides.thinking_budget, Some(16_000));
            assert_eq!(overrides.max_tokens, Some(max_tokens));
            assert_eq!(overrides.reasoning_effort_ordinal, Some(ordinal));
        }
    }

    #[test]
    fn anthropic_effort_levels_map_to_thinking_budgets() {
        for (level, budget, max_tokens) in [
            (ReasoningEffort::Low, 4_096, 16_000),
            (ReasoningEffort::Medium, 16_000, 32_000),
            (ReasoningEffort::High, 63_999, 64_000),
            (ReasoningEffort::XHigh, 127_999, 128_000),
            (ReasoningEffort::Max, 127_999, 128_000),
        ] {
            let overrides = resolve_exec_thinking(Some(level), ProviderFormat::Anthropic).unwrap();
            assert_eq!(overrides.thinking_budget, Some(budget));
            assert_eq!(overrides.max_tokens, Some(max_tokens));
            assert_eq!(overrides.reasoning_effort_ordinal, None);
        }
    }

    #[test]
    fn tool_dispatch_start_maps_to_action_called_with_exact_name() {
        let ev = QueryEvent::ToolDispatchStart {
            tool_use_id: "t1".into(),
            name: "Read".into(),
            input: json!({ "file_path": "REBON.md", "limit": 2 }),
        };
        let out = project_json(&ev);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "action.called");
        assert_eq!(out[0]["callId"], "t1");
        // The registered tool name survives verbatim — the whole reason we
        // observe raw QueryEvents instead of the ACP title/kind projection.
        assert_eq!(out[0]["name"], "Read");
        assert_eq!(out[0]["input"]["file_path"], "REBON.md");
    }

    #[test]
    fn tool_dispatch_result_ok_maps_to_completed() {
        let ev = QueryEvent::ToolDispatchResult {
            tool_use_id: "t1".into(),
            name: "Read".into(),
            outcome: Ok(json!({ "content": "hi" })),
            error_presentation: None,
        };
        let out = project_json(&ev);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "action.result");
        assert_eq!(out[0]["callId"], "t1");
        assert_eq!(out[0]["status"], "completed");
        assert_eq!(out[0]["output"]["content"], "hi");
    }

    #[test]
    fn tool_dispatch_result_err_maps_to_failed_with_message() {
        let ev = QueryEvent::ToolDispatchResult {
            tool_use_id: "t2".into(),
            name: "TaskList".into(),
            outcome: Err("unexpected parameter".into()),
            error_presentation: None,
        };
        let out = project_json(&ev);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "action.result");
        assert_eq!(out[0]["status"], "failed");
        assert_eq!(out[0]["output"], "unexpected parameter");
    }

    #[test]
    fn tool_dispatch_result_err_prefers_display_message() {
        let ev = QueryEvent::ToolDispatchResult {
            tool_use_id: "t3".into(),
            name: "SendMessage".into(),
            outcome: Err("detailed model recovery instructions".into()),
            error_presentation: Some(rebon_tools_core::ToolErrorPresentation::new(
                "agent_closed",
                "Agent is no longer available.",
                "detailed model recovery instructions",
            )),
        };

        let out = project_json(&ev);

        assert_eq!(out[0]["output"], "Agent is no longer available.");
    }

    #[test]
    fn iteration_complete_emits_content_and_an_explicit_turn_boundary() {
        let ev = QueryEvent::IterationComplete {
            iteration: 2,
            message: rebon_api::AssistantMessage {
                id: "msg-1".into(),
                model: "gpt-5.5".into(),
                content: vec![
                    ApiContentBlock::Thinking(rebon_api::ThinkingBlock {
                        thinking: "reasoning".into(),
                        ..Default::default()
                    }),
                    ApiContentBlock::Text(rebon_api::TextBlock {
                        text: "answer".into(),
                    }),
                ],
                stop_reason: Some(rebon_api::StopReason::EndTurn),
                usage: rebon_api::Usage {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_read_input_tokens: 20,
                    ..Default::default()
                },
            },
        };

        let out = project_json(&ev);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["type"], "thinking");
        assert_eq!(out[0]["iteration"], 2);
        assert_eq!(out[1]["type"], "message");
        assert_eq!(out[1]["iteration"], 2);
        assert_eq!(out[2]["type"], "turn.completed");
        assert_eq!(out[2]["iteration"], 2);
        assert_eq!(out[2]["model"], "gpt-5.5");
        assert_eq!(out[2]["stopReason"], "end_turn");
        assert_eq!(out[2]["usage"]["input_tokens"], 120);
        assert_eq!(out[2]["usage"]["cache_read_input_tokens"], 20);
    }

    #[test]
    fn empty_iteration_still_emits_a_turn_boundary() {
        let ev = QueryEvent::IterationComplete {
            iteration: 0,
            message: rebon_api::AssistantMessage {
                id: "msg-2".into(),
                model: "claude-opus-4-8".into(),
                content: Vec::new(),
                stop_reason: Some(rebon_api::StopReason::ToolUse),
                usage: rebon_api::Usage::default(),
            },
        };

        let out = project_json(&ev);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "turn.completed");
        assert_eq!(out[0]["stopReason"], "tool_use");
    }

    #[test]
    fn error_and_limit_and_cancel_map_to_error_events() {
        assert_eq!(
            project_json(&QueryEvent::Error("boom".into()))[0]["message"],
            "boom"
        );
        assert_eq!(project_json(&QueryEvent::Cancelled)[0]["type"], "error");
        let limit = project_json(&QueryEvent::IterationLimitReached { iterations: 5 });
        assert_eq!(limit[0]["type"], "error");
        assert!(limit[0]["message"]
            .as_str()
            .unwrap()
            .contains("iteration limit"));
    }

    #[test]
    fn compaction_events_preserve_their_phase() {
        let started = project_json(&QueryEvent::CompactingStarted { messages_before: 3 });
        let finished = project_json(&QueryEvent::CompactingFinished {
            messages_after: 1,
            used_model: true,
        });
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["type"], "compaction");
        assert_eq!(started[0]["phase"], "started");
        assert_eq!(finished[0]["phase"], "finished");
    }
}

/// Compact human-readable trace for the non-`--json` mode. Goes to stderr for
/// tool activity and stdout for assistant text.
fn project_text(event: &QueryEvent) {
    match event {
        QueryEvent::ToolDispatchStart {
            tool_use_id, name, ..
        } => eprintln!("  → {name} ({tool_use_id})"),
        QueryEvent::ToolDispatchResult { name, outcome, .. } => {
            let ok = if outcome.is_ok() { "ok" } else { "err" };
            eprintln!("  ← {name} [{ok}]");
        }
        QueryEvent::IterationComplete { message, .. } => {
            for block in &message.content {
                if let ApiContentBlock::Text(text) = block {
                    if !text.text.trim().is_empty() {
                        println!("{}", text.text);
                    }
                }
            }
        }
        QueryEvent::Error(message) => eprintln!("error: {message}"),
        _ => {}
    }
}
