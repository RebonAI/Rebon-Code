//! Runtime helpers for local shell tasks, local agent tasks, agent
//! generation, and status output.

use rebon_plugin_tasks::runtime::{TaskSnapshot, TaskStatus};
#[cfg(test)]
use rebon_types::UltraplanStage;
use rebon_types::{
    ReviewSummary, ReviewerFinding, ReviewerFindingClass, ReviewerVerdictRecord, RunPhase,
    StructuredReview, UltraplanDiagnosticClass, UltraplanEvidenceReference, VerdictSource,
};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;
use rebon_plugin_agents::dialog::AgentsDialogState;

use crate::session::ultraplan_review::{
    retry_review, reviewer_failure_fingerprint, reviewer_failure_prefix, ReviewKind,
};
use crate::session::ultraplan_run::{
    ingest_reviewer_failure, persist_reviewer_retry_failure, synchronize_ultraplan_task_budget,
    ultraplan_context_for_phase, validate_structured_reviewer_result,
};

/// the runtime: register
/// the task, then let the coordinator's observation stream drive the
/// registry updates. The returned receiver is dropped immediately; the
/// registry snapshot polled every frame is the display authority.
///
/// Task ids sync point the runtime convention `shell-{startTimeMs}` which keeps
/// them unique within a session.
pub(super) fn spawn_shell_task(
    app: &mut AppState,
    session: &TuiEngineSession,
    command: &str,
) -> anyhow::Result<String> {
    use rebon_plugin_tasks::runtime::{spawn_local_shell_task, LocalShellTaskSpec};
    let id = format!("shell-{}", rebon_types::wall_clock_ms_u128());
    let tool_name = shell_tool_name();
    let input = serde_json::json!({ "command": command });
    let spec = LocalShellTaskSpec::new(id.clone(), tool_name, input);
    let registry = (*session.engine_half.tasks).clone();
    let _observations = spawn_local_shell_task(session.engine_half.engine.clone(), registry, spec)?;
    let snapshots = app.task_snapshots();
    app.background_tasks_dialog = crate::tui::ui_registry::open_background_tasks(
        rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogOpen {
            snapshots,
            initial_detail_task_id: Some(id.clone()),
            foregrounded_task_id: app.foregrounded_task_id.clone(),
            kind_filter: None,
        },
    );
    Ok(id)
}

pub(super) fn task_notification_scan_needed(
    subscription: &mut Option<tokio::sync::watch::Receiver<u64>>,
    current: tokio::sync::watch::Receiver<u64>,
) -> bool {
    let revision = match subscription {
        Some(revision) if revision.same_channel(&current) => revision,
        _ => {
            let mut current = current;
            let _ = current.borrow_and_update();
            *subscription = Some(current);
            // Results may already exist before a resumed session is subscribed.
            return true;
        }
    };
    if revision.has_changed().unwrap_or(false) {
        let _ = revision.borrow_and_update();
        true
    } else {
        false
    }
}

/// App snapshots share the pending work, never duplicate its result receivers.
#[derive(Clone, Default)]
pub(crate) struct InlineShellCommands(Arc<std::sync::Mutex<Vec<InlineShellCommand>>>);

impl std::fmt::Debug for InlineShellCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineShellCommands")
            .finish_non_exhaustive()
    }
}

struct InlineShellCommand {
    uuid: String,
    command_line: String,
    started: std::time::Instant,
    elapsed_seconds: u64,
    result: oneshot::Receiver<String>,
    feedback: Option<crate::session::submit_payload::SubmitPayload>,
    runtime: Arc<crate::session::runtime::SessionRuntime>,
    state: Arc<rebon_acp::ServerState>,
    retry_at: Option<std::time::Instant>,
    persisted: bool,
}

pub(super) fn spawn_inline_shell_command(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    command: &str,
    command_line: &str,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let uuid = format!(
        "s-shell-{}-{}-{}",
        session.session_id,
        rebon_types::wall_clock_ms_u128(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
    );
    let (tx, result) = oneshot::channel();
    let pending = InlineShellCommand {
        uuid: uuid.clone(),
        command_line: command_line.to_string(),
        started: std::time::Instant::now(),
        elapsed_seconds: 0,
        result,
        feedback: None,
        runtime: session.engine_half.runtime.clone(),
        state: session.server_state.clone(),
        retry_at: None,
        persisted: false,
    };
    // Projection only: completed feedback uses the attachment/replay path below.
    let content =
        super::transcript_messages::format_command_feedback_text(command_line, "Running… (0s)");
    super::with_main_agent_view(app, |app| {
        app.rebon_tui
            .transcript
            .upsert(rebon_tui::Message::System(rebon_tui::SystemMessage {
                uuid,
                timestamp: rebon_types::format_system_time_iso_ms(std::time::SystemTime::now()),
                subtype: "local_command".into(),
                content: Some(content),
                level: Some(rebon_tui::SystemLevel::Info),
                is_meta: None,
            }));
    });
    app.inline_shell_commands
        .0
        .lock()
        .expect("inline shells poisoned")
        .push(pending);

    let input = serde_json::json!({ "command": command });
    let context = rebon_tool::ToolContext::new()
        .with_cwd(session.cwd.clone())
        .with_session_id(session.session_id.clone())
        .with_permission_broker(Arc::new(rebon_tool::AutoApprovePermissionBroker));
    let engine = session.engine_half.engine.clone();
    handle.spawn(async move {
        let output = match engine.invoke_tool(shell_tool_name(), input, &context).await {
            Ok(value) => format_shell_tool_result(&value),
            Err(err) => format!("Command failed: {err}"),
        };
        let _ = tx.send(output);
    });
}

/// Keep unfinished rows in the inline viewport rather than immutable scrollback.
pub(super) fn inline_shell_live_prefix(app: &AppState) -> usize {
    let commands = app
        .inline_shell_commands
        .0
        .lock()
        .expect("inline shells poisoned");
    app.rebon_tui
        .transcript
        .rows()
        .iter()
        .position(|row| {
            commands
                .iter()
                .any(|shell| shell.feedback.is_none() && row.uuid() == Some(shell.uuid.as_str()))
        })
        .unwrap_or(app.rebon_tui.transcript.len())
}

pub(super) fn drain_inline_shell_commands(
    app: &mut AppState,
    session: &TuiEngineSession,
    allow_idle_save: bool,
) {
    let now = std::time::Instant::now();
    let pending = app.inline_shell_commands.clone();
    let mut commands = pending.0.lock().expect("inline shells poisoned");
    commands.retain_mut(|shell| {
        let mut changed = None;
        if shell.feedback.is_none() {
            let output = match shell.result.try_recv() {
                Ok(output) => Some(output),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Closed) => Some("Command failed: shell task ended without a result".into()),
            };
            if let Some(output) = output {
                let text = super::transcript_messages::format_command_feedback_text(&shell.command_line, &output);
                let mut feedback = crate::session::submit_payload::SubmitPayload {
                    text: text.clone(),
                    model_text: Some(format!("The user ran a local shell command. Command and output:\n{text}")),
                    user_message_uuid: Some(shell.uuid.clone()),
                    image_pastes: Vec::new(),
                    directory_attachments: Vec::new(),
                    execution_policy: None,
                    skill_invocations: Vec::new(),
                };
                shell.runtime.mid_turn_queue.enqueue_submit(&mut feedback);
                shell.feedback = Some(feedback);
                changed = Some(text);
            } else {
                let elapsed = now.duration_since(shell.started).as_secs();
                if elapsed != shell.elapsed_seconds {
                    shell.elapsed_seconds = elapsed;
                    changed = Some(super::transcript_messages::format_command_feedback_text(
                        &shell.command_line, &format!("Running… ({elapsed}s)"),
                    ));
                }
            }
        }
        // A session switch must not redirect a late completion into its new view.
        if shell.runtime.session_id == session.session_id {
            if let Some(text) = changed {
                super::with_main_agent_view(app, |app| {
                    if let Some(rebon_tui::Message::System(row)) = app.rebon_tui.transcript.get(&shell.uuid) {
                        let mut row = row.clone();
                        row.content = Some(text);
                        app.rebon_tui.transcript.upsert(rebon_tui::Message::System(row));
                    }
                });
            }
        }
        let Some(feedback) = &shell.feedback else { return true; };
        if !allow_idle_save || shell.retry_at.is_some_and(|retry| now < retry) { return true; }
        if !shell.persisted {
            match shell.runtime.mid_turn_queue.persist_idle_submit(
                &shell.runtime.projects_root, &shell.runtime.cwd, &shell.state, feedback,
            ) {
                Ok(false) => return true,
                Ok(true) => shell.persisted = true,
                Err(err) => {
                    tracing::error!(uuid = %shell.uuid, error = %err, "could not save shell feedback for replay");
                    if shell.retry_at.is_none() && shell.runtime.session_id == session.session_id {
                        super::inject_system_message(app, "error", &format!("Shell output could not be saved to agent context; retrying: {err}"));
                    }
                    shell.retry_at = Some(now + std::time::Duration::from_secs(1));
                    return true;
                }
            }
        }
        if shell.runtime.session_id != session.session_id {
            return true;
        }
        super::with_main_agent_view(app, |app| {
            app.deferred_internal_submit_payloads.push(crate::session::submit_payload::SubmitPayload {
                text: format!(
                    "Respond to the result of the user's local shell command `{}` (transcript message {}). The command has already finished and its output is in the conversation. Do not run it again just to obtain the result.",
                    shell.command_line, shell.uuid,
                ),
                model_text: None,
                user_message_uuid: None,
                image_pastes: Vec::new(),
                directory_attachments: Vec::new(),
                execution_policy: None,
                skill_invocations: Vec::new(),
            });
        });
        false
    });
}

fn shell_tool_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "PowerShell"
    } else {
        "Bash"
    }
}

fn format_shell_tool_result(value: &serde_json::Value) -> String {
    let stdout = value
        .get("stdout")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim_end();
    let stderr = value
        .get("stderr")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim_end();
    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(stderr);
    }
    if output.is_empty() {
        output.push_str("(no output)");
    }
    if value
        .get("timedOut")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        output.push_str("\n[timed out]");
    } else if let Some(code) = value.get("exitCode").and_then(|value| value.as_i64()) {
        if code != 0 {
            output.push_str(&format!("\n[exit code: {code}]"));
        }
    }
    output
}

/// Spawn a foreground sub-agent through the same detached worker runtime used
/// by Agent tool workers, then open the task dialog for the new task.
pub(super) fn spawn_agent_task(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    prompt: &str,
) -> anyhow::Result<String> {
    use rebon_tool::SubAgentSpec;

    let id = format!("agent-{}", rebon_types::wall_clock_ms_u128());
    let mut spec = SubAgentSpec::new(prompt.to_string());
    spec.model = Some(session.model.default_name.clone());
    spec.run_in_background = false;
    spec.permission_prompts_unavailable = false;
    spec.cwd = Some(session.cwd.clone());
    spec.permission_broker = Some(session.engine_half.runtime.permission_broker.clone());
    spec.metadata = serde_json::json!({
        "agent_id": id,
        "agent_type": "general-purpose",
        "description": prompt.lines().next().unwrap_or_default(),
        "parent_session_id": session.session_id,
    });

    let spawned_id = {
        let _guard = handle.enter();
        handle
            .block_on(session.engine_half.sub_agent_spawner.spawn_detached(spec))
            .map_err(anyhow::Error::msg)?
    };
    debug_assert_eq!(spawned_id, id);
    let snapshots = app.task_snapshots();
    app.background_tasks_dialog = crate::tui::ui_registry::open_background_tasks(
        rebon_plugin_tasks::ui::background_tasks_dialog::BackgroundTasksDialogOpen {
            snapshots,
            initial_detail_task_id: Some(spawned_id.clone()),
            foregrounded_task_id: app.foregrounded_task_id.clone(),
            kind_filter: None,
        },
    );
    Ok(spawned_id)
}

pub(super) fn drain_ultraplan_reviewer_verdicts(app: &mut AppState, session: &TuiEngineSession) {
    let Some(active_run_id) = app
        .ultraplan_status
        .as_ref()
        .map(|status| status.run_id.clone())
    else {
        return;
    };

    let snapshots = app.task_snapshots();
    let research_agents_used = snapshots
        .iter()
        .filter(|snapshot| {
            snapshot.ultraplan_id() == Some(active_run_id.as_str())
                && snapshot.ultraplan_role() == Some("researcher")
        })
        .count() as u32;
    let adversarial_reviews_used = snapshots
        .iter()
        .filter(|snapshot| {
            snapshot.ultraplan_id() == Some(active_run_id.as_str())
                && snapshot.ultraplan_role() == Some("reviewer")
                && snapshot
                    .metadata
                    .get("ultraplan_retry_attempt")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
        })
        .count() as u32;

    synchronize_ultraplan_task_budget(
        session,
        &active_run_id,
        research_agents_used,
        adversarial_reviews_used,
    );

    for snapshot in snapshots {
        if !is_eligible_ultraplan_reviewer_snapshot(&snapshot, &active_run_id) {
            continue;
        }
        let task_id = snapshot.id.to_string();
        if app
            .ingested_ultraplan_reviewer_task_ids
            .contains(task_id.as_str())
        {
            continue;
        }

        let mut saved_state = None;
        let mut saved_retry_request = None;
        for attempt in 0..=1 {
            let Some(mut state) = rebon_session::load_ultraplan_run(
                &session.projects_root,
                &session.cwd,
                &active_run_id,
            ) else {
                tracing::warn!(
                    task_id = %task_id,
                    run_id = %active_run_id,
                    "completed ultraplan reviewer task could not be ingested because run state was missing"
                );
                break;
            };
            let expected_revision = state.state_revision;
            state.synchronize_worker_budget_usage(research_agents_used, adversarial_reviews_used);
            let mut retry_request = None;

            match structured_reviewer_result(&snapshot)
                .and_then(|review| validate_structured_reviewer_result(&snapshot, &state, review))
            {
                Ok(review) => {
                    if let Some(plan_hash) = snapshot.metadata_str("plan_hash") {
                        state.clear_tool_error_attempts(&reviewer_failure_prefix(
                            &state.run_id,
                            plan_hash,
                        ));
                    }
                    ingest_structured_review(&snapshot, &mut state, review);
                }
                Err((class, message)) => {
                    let plan_hash = snapshot
                        .metadata_str("plan_hash")
                        .map(str::to_string)
                        .or_else(|| state.plan_hash.clone());
                    let retryable = matches!(
                        class,
                        UltraplanDiagnosticClass::ReviewerUnavailable
                            | UltraplanDiagnosticClass::ReviewerMalformed
                    );
                    let already_retried = snapshot
                        .metadata
                        .get("ultraplan_retry_attempt")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true);
                    let attempts = plan_hash.as_deref().map(|plan_hash| {
                        state.record_tool_error_attempts(
                            reviewer_failure_fingerprint(&state.run_id, plan_hash, class, &message),
                            1,
                        )
                    });
                    if retryable
                        && !already_retried
                        && attempts
                            .is_some_and(|attempts| attempts <= state.budget.max_tool_error_retries)
                    {
                        if let (Some(plan), Some(plan_hash)) =
                            (state.last_plan_draft.clone(), plan_hash)
                        {
                            let kind = if snapshot.metadata_str("ultraplan_review_kind")
                                == Some("manual")
                            {
                                ReviewKind::Manual
                            } else {
                                ReviewKind::Auto
                            };
                            state.write_checkpoint(
                                Vec::new(),
                                "retry the reviewer once for the same structured-output failure",
                            );
                            retry_request = Some((plan, plan_hash, kind, task_id.clone()));
                        } else {
                            ingest_reviewer_failure(&snapshot, &mut state, class, message);
                        }
                    } else {
                        ingest_reviewer_failure(&snapshot, &mut state, class, message);
                    }
                }
            }
            state.updated_at_ms = (rebon_types::wall_clock_ms_u128() as u64)
                .max(state.updated_at_ms.saturating_add(1));
            match rebon_session::save_ultraplan_run_cas(
                &session.projects_root,
                &session.cwd,
                expected_revision,
                &state,
            ) {
                Ok(()) => {
                    saved_retry_request = retry_request;
                    saved_state = Some(
                        rebon_session::load_ultraplan_run(
                            &session.projects_root,
                            &session.cwd,
                            &active_run_id,
                        )
                        .unwrap_or(state),
                    );
                    break;
                }
                Err(rebon_session::UltraplanRunStoreError::StaleRevision { .. })
                    if attempt == 0 =>
                {
                    continue;
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        task_id = %task_id,
                        run_id = %active_run_id,
                        "failed to persist ultraplan reviewer result"
                    );
                    break;
                }
            }
        }
        let Some(mut state) = saved_state else {
            continue;
        };
        if let Some((plan, plan_hash, kind, retry_of)) = saved_retry_request {
            match retry_review(session, &state, &plan, &plan_hash, kind, &retry_of) {
                Ok(()) => {
                    if let Some(status) = app.ultraplan_status.as_mut() {
                        status.round = state.round.max(1);
                        status.phase = crate::session::ultraplan_run::UltraplanPhase::Reviewing;
                        status.context = ultraplan_context_for_phase(
                            &state.run_id,
                            status.phase,
                            state.manifest.clone(),
                        )
                        .map(|context| context.with_run_head(&state.head()));
                    }
                    app.ingested_ultraplan_reviewer_task_ids.insert(task_id);
                    continue;
                }
                Err(diagnostic) => {
                    state = persist_reviewer_retry_failure(
                        session,
                        &snapshot,
                        &active_run_id,
                        diagnostic,
                    )
                    .unwrap_or(state);
                }
            }
        }

        if let Some(status) = app.ultraplan_status.as_mut() {
            status.round = state.round.max(1);
            status.last_verdict = state.reviewer_verdicts.last().cloned();
            status.last_coverage = state.last_coverage.clone();
            // The ingest paths set state.phase authoritatively; deriving the
            // UI phase from the workflow stage would contradict it after a
            // reviewer failure (phase Synthesizing, stage back to drafting).
            status.phase = crate::session::ultraplan_run::status_phase_from_run_phase(state.phase)
                .unwrap_or(crate::session::ultraplan_run::UltraplanPhase::Researching);
            if status.phase != crate::session::ultraplan_run::UltraplanPhase::Executing {
                status.context = ultraplan_context_for_phase(
                    &state.run_id,
                    status.phase,
                    state.manifest.clone(),
                )
                .map(|context| context.with_run_head(&state.head()));
            }
        }
        app.ingested_ultraplan_reviewer_task_ids.insert(task_id);
    }
}

fn structured_reviewer_result(
    snapshot: &TaskSnapshot,
) -> Result<StructuredReview, (UltraplanDiagnosticClass, String)> {
    if snapshot.status != TaskStatus::Completed {
        return Err((
            UltraplanDiagnosticClass::ReviewerUnavailable,
            snapshot.error.clone().unwrap_or_else(|| {
                format!("reviewer task ended with status {:?}", snapshot.status)
            }),
        ));
    }
    let result = snapshot.result.as_ref().ok_or_else(|| {
        (
            UltraplanDiagnosticClass::ReviewerMalformed,
            "reviewer task completed without a result payload".to_string(),
        )
    })?;
    let value = result
        .get("structured_output")
        .filter(|value| !value.is_null())
        .or_else(|| {
            result
                .get("diagnostics")
                .and_then(|value| value.get("structured_output"))
                .and_then(|value| value.get("accepted"))
                .filter(|value| !value.is_null())
        })
        .cloned()
        .ok_or_else(|| {
            (
                UltraplanDiagnosticClass::ReviewerMalformed,
                "reviewer did not return a StructuredOutput result".to_string(),
            )
        })?;
    serde_json::from_value(value).map_err(|err| {
        (
            UltraplanDiagnosticClass::ReviewerMalformed,
            format!("reviewer StructuredOutput could not be decoded: {err}"),
        )
    })
}

fn ingest_structured_review(
    snapshot: &TaskSnapshot,
    state: &mut rebon_types::UltraplanRunState,
    review: StructuredReview,
) {
    state.pending_review_plan_hash = None;
    let plan_hash = review.plan_hash.clone();
    let passed = review.is_pass();
    let mut blockers = review
        .blockers()
        .map(|finding| finding.message.clone())
        .collect::<Vec<_>>();
    blockers.extend(
        review
            .step_coverage
            .iter()
            .filter(|coverage| !coverage.ok)
            .map(|coverage| format!("{}: {}", coverage.step_id, coverage.reason)),
    );
    let summary = ReviewSummary {
        plan_hash: plan_hash.clone(),
        verdict: if passed { "PASS" } else { "FAIL" }.into(),
        source: VerdictSource::Agent,
        blockers: blockers.clone(),
        coverage: review.step_coverage.clone(),
    };
    let record = ReviewerVerdictRecord {
        round: state.round.max(1),
        verdict: summary.verdict.clone(),
        blocking_gaps: blockers.len() as u32,
        source: VerdictSource::Agent,
    };
    let evidence = review_evidence(&review);

    state.record_structured_review(review.clone());
    state.last_review_summary = Some(summary);
    state.reviewer_verdicts.push(record);
    match snapshot.metadata_str("ultraplan_review_kind") {
        Some("manual") if !passed => {
            state.manual_review_failed_hash = Some(plan_hash.clone());
        }
        Some("manual") => {
            if state.manual_review_failed_hash.as_deref() == Some(plan_hash.as_str()) {
                state.manual_review_failed_hash = None;
            }
        }
        _ if passed => {
            state.auto_review_passed_hash = Some(plan_hash.clone());
            if state.manual_review_failed_hash.as_deref() == Some(plan_hash.as_str()) {
                state.manual_review_failed_hash = None;
            }
        }
        _ => {
            if state.auto_review_passed_hash.as_deref() == Some(plan_hash.as_str()) {
                state.auto_review_passed_hash = None;
                state.released_plan_hash = None;
                state.interview.confirmation = None;
            }
        }
    }

    if review
        .requirements_patch
        .as_ref()
        .is_some_and(|patch| !patch.items.is_empty())
    {
        match state.apply_pending_requirements_patch() {
            Ok(true) => {
                // The applied patch wiped the plan bindings, which is what
                // moves the derived stage back to drafting. Nothing gates on
                // that stage any more; it keeps the status line honest.
                state.phase = RunPhase::Researching;
                state.write_checkpoint(
                    evidence,
                    "verify the batched reviewer requirements patch and revise the plan",
                );
                return;
            }
            Ok(false) => {}
            Err(err) => {
                let mut conflict_review = review.clone();
                conflict_review.requirements_patch = None;
                conflict_review.findings.push(ReviewerFinding {
                    classification: ReviewerFindingClass::Blocker,
                    code: "requirements_patch_conflict".into(),
                    message: err.to_string(),
                    evidence: Vec::new(),
                });
                state.record_structured_review(conflict_review);
                state.last_review_summary = Some(ReviewSummary {
                    plan_hash: plan_hash.clone(),
                    verdict: "FAIL".into(),
                    source: VerdictSource::Agent,
                    blockers: vec![err.to_string()],
                    coverage: review.step_coverage.clone(),
                });
            }
        }
    }

    state.phase = RunPhase::Synthesizing;
    state.write_checkpoint(
        evidence,
        if passed {
            "present the reviewed draft to the user, then run the final gate"
        } else {
            "resolve the reviewer blockers before final delivery"
        },
    );
}

fn review_evidence(review: &StructuredReview) -> Vec<UltraplanEvidenceReference> {
    review
        .findings
        .iter()
        .flat_map(|finding| {
            finding
                .evidence
                .iter()
                .map(move |location| UltraplanEvidenceReference {
                    location: location.clone(),
                    claim: finding.message.clone(),
                })
        })
        .collect()
}

fn is_eligible_ultraplan_reviewer_snapshot(snapshot: &TaskSnapshot, active_run_id: &str) -> bool {
    snapshot.status.is_terminal()
        && snapshot.ultraplan_role() == Some("reviewer")
        && snapshot.ultraplan_id() == Some(active_run_id)
}

/// Spawn an async LLM call for the agents wizard Generate step.
pub(super) fn spawn_agent_generation(
    app: &mut AppState,
    session: &TuiEngineSession,
    handle: &Handle,
    agent_gen_rx: &mut Option<
        oneshot::Receiver<Result<rebon_plugin_agents::surface::generate::GeneratedAgent, String>>,
    >,
    user_prompt: &str,
) {
    let Some(dialog) = app.dialogs.top_as::<AgentsDialogState>() else {
        return;
    };
    let existing = dialog.existing_identifiers();
    let request = rebon_plugin_agents::surface::generate::build_generation_request(
        user_prompt,
        &existing,
        true,
    );
    let client = session.engine_half.client.clone();
    let model = session.model.default_name.clone();
    let (tx, rx) = oneshot::channel();
    *agent_gen_rx = Some(rx);

    let _guard = handle.enter();
    tokio::spawn(async move {
        let api_request = rebon_api::CreateMessageRequest::simple(model, &request.user_prompt)
            .with_system(request.system_prompt)
            .with_max_tokens(4096);
        let result = match client.create_message(api_request).await {
            Ok(msg) => {
                let text = msg.text();
                if text.is_empty() {
                    Err("Model returned an empty response.".to_string())
                } else {
                    rebon_plugin_agents::surface::generate::parse_generated_agent(&text).map_err(|e| match e {
                        rebon_plugin_agents::surface::generate::ParseError::NoJsonObject => {
                            "No JSON object found in model response.".to_string()
                        }
                        rebon_plugin_agents::surface::generate::ParseError::InvalidJson(detail) => {
                            format!("Invalid JSON in model response: {detail}")
                        }
                        rebon_plugin_agents::surface::generate::ParseError::InvalidConfiguration => {
                            "Generated agent is missing required fields.".to_string()
                        }
                    })
                }
            }
            Err(err) => Err(format!("API error: {err}")),
        };
        let _ = tx.send(result);
    });
}

/// Poll the agent-generation oneshot each frame. If a result has
/// arrived, feed it into the agents dialog.
pub(super) fn drain_agent_generation(
    app: &mut AppState,
    agent_gen_rx: &mut Option<
        oneshot::Receiver<Result<rebon_plugin_agents::surface::generate::GeneratedAgent, String>>,
    >,
) {
    let Some(rx) = agent_gen_rx.as_mut() else {
        return;
    };
    match rx.try_recv() {
        Ok(Ok(agent)) => {
            if let Some(dialog) = app.dialogs.top_as_mut::<AgentsDialogState>() {
                dialog.apply_generation_result(agent);
            }
            *agent_gen_rx = None;
        }
        Ok(Err(err)) => {
            if let Some(dialog) = app.dialogs.top_as_mut::<AgentsDialogState>() {
                dialog.apply_generation_error(&err);
            }
            *agent_gen_rx = None;
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Closed) => {
            if let Some(dialog) = app.dialogs.top_as_mut::<AgentsDialogState>() {
                dialog.apply_generation_error("Generation task was cancelled.");
            }
            *agent_gen_rx = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rebon_plugin_tasks::runtime::{LocalAgentData, TaskData, TaskId, TaskKind, TaskSnapshot};
    use rebon_types::{
        CapabilityContext, ExecutionCard, NetworkCapability, PlanCoverageResult, PromptCancel,
        UltraplanRunState,
    };

    use super::*;
    use crate::session::ultraplan_run::{UltraplanPhase, UltraplanStatus};
    use crate::tui::app::AppState;
    use crate::tui::runner::test_support::make_test_tui_session;

    #[test]
    fn task_notification_subscription_follows_new_session_terminal_results() {
        for status in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Killed,
        ] {
            let previous = AppState::default();
            let current = AppState::default();
            let mut subscription = None;
            task_notification_scan_needed(
                &mut subscription,
                previous.tasks.subscribe_notification_revision(),
            );
            task_notification_scan_needed(
                &mut subscription,
                current.tasks.subscribe_notification_revision(),
            );
            insert_terminal_snapshot(
                &current,
                "worker",
                status,
                serde_json::json!({}),
                (status == TaskStatus::Failed).then(|| "connection reset".into()),
                None,
            );
            assert!(
                task_notification_scan_needed(
                    &mut subscription,
                    current.tasks.subscribe_notification_revision(),
                ),
                "{status:?} must wake the resumed session"
            );
            assert!(!task_notification_scan_needed(
                &mut subscription,
                current.tasks.subscribe_notification_revision(),
            ));
        }
    }

    #[test]
    fn task_notification_subscription_scans_existing_results_on_rebind() {
        let previous = AppState::default();
        let current = AppState::default();
        let mut subscription = None;
        task_notification_scan_needed(
            &mut subscription,
            previous.tasks.subscribe_notification_revision(),
        );
        insert_terminal_snapshot(
            &current,
            "worker",
            TaskStatus::Failed,
            serde_json::json!({}),
            Some("connection reset".into()),
            None,
        );
        assert!(task_notification_scan_needed(
            &mut subscription,
            current.tasks.subscribe_notification_revision(),
        ));
        assert_eq!(current.tasks.unnotified_notifications().len(), 1);
    }

    #[test]
    fn task_notification_subscription_keeps_unread_revision_without_repeating() {
        let current = AppState::default();
        let mut subscription = None;
        task_notification_scan_needed(
            &mut subscription,
            current.tasks.subscribe_notification_revision(),
        );
        insert_terminal_snapshot(
            &current,
            "worker",
            TaskStatus::Completed,
            serde_json::json!({}),
            None,
            None,
        );
        assert!(task_notification_scan_needed(
            &mut subscription,
            current.tasks.subscribe_notification_revision(),
        ));
        assert!(!task_notification_scan_needed(
            &mut subscription,
            current.tasks.subscribe_notification_revision(),
        ));
    }

    #[test]
    fn task_notification_subscription_ignores_previous_session() {
        let previous = AppState::default();
        let current = AppState::default();
        let mut subscription = None;
        task_notification_scan_needed(
            &mut subscription,
            previous.tasks.subscribe_notification_revision(),
        );
        task_notification_scan_needed(
            &mut subscription,
            current.tasks.subscribe_notification_revision(),
        );
        insert_terminal_snapshot(
            &previous,
            "worker",
            TaskStatus::Failed,
            serde_json::json!({}),
            Some("connection reset".into()),
            None,
        );
        assert!(!task_notification_scan_needed(
            &mut subscription,
            current.tasks.subscribe_notification_revision(),
        ));
        assert_eq!(previous.tasks.unnotified_notifications().len(), 1);
    }

    struct GatedShellTool {
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl rebon_tool::Tool for GatedShellTool {
        fn id(&self) -> rebon_tools_core::ToolId {
            rebon_tools_core::ToolId::new(shell_tool_name())
        }
        fn description(&self) -> &str {
            "Deterministic shell fixture; executes no process"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _context: &rebon_tool::ToolContext,
        ) -> rebon_tools_core::ToolResult<serde_json::Value> {
            self.release.notified().await;
            Ok(
                serde_json::json!({"stdout": "fixture stdout\n", "stderr": "fixture stderr", "exitCode": 7}),
            )
        }
    }

    async fn wait_for_shell_result(app: &AppState) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if app
                    .inline_shell_commands
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|shell| !shell.result.is_empty())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shell completion");
    }

    fn shell_text(app: &AppState, index: usize) -> &str {
        match &app.rebon_tui.transcript.rows()[index] {
            rebon_tui::Message::System(row) => row.content.as_deref().unwrap(),
            other => panic!("expected shell row, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inline_shell_is_immediate_ticks_and_replays_without_duplicate_rows() {
        let root = tempfile::tempdir().unwrap();
        let mut session = make_test_tui_session();
        Arc::get_mut(&mut session.engine_half.runtime)
            .unwrap()
            .projects_root = root.path().into();
        let tool = Arc::new(GatedShellTool {
            release: tokio::sync::Notify::new(),
        });
        let mut engine = rebon_core::Engine::new();
        engine.register_tool(tool.clone());
        session.engine_half.engine = Arc::new(engine);
        let mut app = AppState::new();
        let turn = session.engine_half.runtime.mid_turn_queue.begin_turn();
        spawn_inline_shell_command(
            &mut app,
            &session,
            &Handle::current(),
            "fixture",
            "!fixture",
        );
        assert_eq!(shell_text(&app, 0), "!fixture\n⎿ Running… (0s)");
        assert_eq!(inline_shell_live_prefix(&app), 0);
        app.inline_shell_commands.0.lock().unwrap()[0].started =
            std::time::Instant::now() - std::time::Duration::from_secs(5);
        drain_inline_shell_commands(&mut app, &session, false);
        assert_eq!(shell_text(&app, 0), "!fixture\n⎿ Running… (5s)");
        tool.release.notify_one();
        wait_for_shell_result(&app).await;
        drain_inline_shell_commands(&mut app, &session, true);
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert!(shell_text(&app, 0).contains("fixture stdout\n  fixture stderr\n  [exit code: 7]"));
        assert_eq!(inline_shell_live_prefix(&app), 1);
        assert_eq!(session.engine_half.runtime.mid_turn_queue.pending_len(), 1);
        assert!(app.deferred_internal_submit_payloads.is_empty());
        let uuid = app.rebon_tui.transcript.rows()[0]
            .uuid()
            .expect("shell row UUID")
            .to_string();
        let visible_output = shell_text(&app, 0).to_string();
        for _ in 0..2 {
            crate::tui::update::translate_session_update(
                &mut app,
                rebon_types::SessionUpdateParams {
                    session_id: session.session_id.clone(),
                    update: rebon_types::SessionUpdate::QueuedUserMessage {
                        uuid: uuid.clone(),
                        content: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                            text: visible_output.clone(),
                            annotations: None,
                        })],
                        image_paste_ids: None,
                    },
                },
            );
        }
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert_eq!(shell_text(&app, 0), visible_output);
        let path =
            rebon_session::transcript_file_path(root.path(), &session.cwd, &session.session_id);
        assert!(!path.exists(), "an active turn still owns the transcript");
        drop(turn);
        drain_inline_shell_commands(&mut app, &session, true);
        drain_inline_shell_commands(&mut app, &session, true);
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert!(app.inline_shell_commands.0.lock().unwrap().is_empty());
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);
        let transcript = rebon_session::load_transcript_from_file(&path)
            .unwrap()
            .unwrap();
        assert_eq!(transcript.messages.len(), 1);
        let replay = rebon_core::query::transcript_to_api_messages(&transcript.messages);
        let text = serde_json::to_string(&replay).unwrap();
        assert!(text.contains("The user ran a local shell command"));
        assert!(text.contains("fixture stdout"));
        assert!(text.contains("exit code: 7"));
    }

    #[tokio::test]
    async fn inline_shell_failure_is_durable_and_repeated_commands_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        let mut session = make_test_tui_session();
        Arc::get_mut(&mut session.engine_half.runtime)
            .unwrap()
            .projects_root = root.path().into();
        let mut app = AppState::new();
        // The session fixture attaches the process tool seat. Replace its engine
        // with a bare one so this tests a missing-tool error, never a real shell.
        session.engine_half.engine = Arc::new(rebon_core::Engine::new());
        for _ in 0..2 {
            spawn_inline_shell_command(
                &mut app,
                &session,
                &Handle::current(),
                "fixture",
                "!fixture",
            );
        }
        assert_ne!(
            app.rebon_tui.transcript.rows()[0].uuid(),
            app.rebon_tui.transcript.rows()[1].uuid()
        );
        wait_for_shell_result(&app).await;
        drain_inline_shell_commands(&mut app, &session, true);
        drain_inline_shell_commands(&mut app, &session, true);
        assert_eq!(app.rebon_tui.transcript.len(), 2);
        assert_eq!(app.deferred_internal_submit_payloads.len(), 2);
        assert_ne!(
            app.deferred_internal_submit_payloads[0].text,
            app.deferred_internal_submit_payloads[1].text
        );
        assert!(shell_text(&app, 0).contains("Command failed:"));
        let path =
            rebon_session::transcript_file_path(root.path(), &session.cwd, &session.session_id);
        let transcript = rebon_session::load_transcript_from_file(&path)
            .unwrap()
            .unwrap();
        assert_eq!(transcript.messages.len(), 2);
        let replay = rebon_core::query::transcript_to_api_messages(&transcript.messages);
        assert_eq!(
            serde_json::to_string(&replay)
                .unwrap()
                .matches("Command failed:")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn inline_shell_reply_waits_for_originating_session_without_duplicate_persistence() {
        let root = tempfile::tempdir().unwrap();
        let mut session = make_test_tui_session();
        Arc::get_mut(&mut session.engine_half.runtime)
            .unwrap()
            .projects_root = root.path().into();
        session.engine_half.engine = Arc::new(rebon_core::Engine::new());
        let mut app = AppState::new();
        spawn_inline_shell_command(
            &mut app,
            &session,
            &Handle::current(),
            "fixture",
            "!fixture",
        );
        wait_for_shell_result(&app).await;
        let mut other = make_test_tui_session();
        other
            .swap_runtime("other-session", "/other", false)
            .unwrap();
        for _ in 0..2 {
            drain_inline_shell_commands(&mut app, &other, true);
            assert!(app.deferred_internal_submit_payloads.is_empty());
            assert_eq!(app.inline_shell_commands.0.lock().unwrap().len(), 1);
        }
        let path =
            rebon_session::transcript_file_path(root.path(), &session.cwd, &session.session_id);
        let saved = rebon_session::load_transcript_from_file(&path)
            .unwrap()
            .unwrap();
        assert_eq!(saved.messages.len(), 1);
        drain_inline_shell_commands(&mut app, &session, true);
        drain_inline_shell_commands(&mut app, &session, true);
        assert_eq!(app.deferred_internal_submit_payloads.len(), 1);
        assert!(app.inline_shell_commands.0.lock().unwrap().is_empty());
        let saved = rebon_session::load_transcript_from_file(&path)
            .unwrap()
            .unwrap();
        assert_eq!(saved.messages.len(), 1);
    }

    #[derive(Default)]
    struct RecordingSubAgentSpawner {
        specs: Mutex<Vec<rebon_tool::SubAgentSpec>>,
        spawned: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl rebon_tool::SubAgentSpawner for RecordingSubAgentSpawner {
        async fn spawn(
            &self,
            _spec: rebon_tool::SubAgentSpec,
        ) -> Result<rebon_tool::SubAgentResult, String> {
            Err("synchronous spawn is not used by this test".into())
        }

        async fn spawn_background(&self, spec: rebon_tool::SubAgentSpec) -> Result<String, String> {
            self.specs.lock().unwrap().push(spec);
            self.spawned.notify_one();
            Ok("recorded-reviewer".into())
        }
    }

    #[test]
    fn completed_matching_reviewer_persists_once_and_updates_status() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        let review_state = load_run(&session, "run-a");
        insert_reviewer_snapshot(
            &app,
            "reviewer-1",
            &review_state,
            pass_review(&review_state),
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);
        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, "run-a")
                .expect("run state");
        assert_eq!(state.reviewer_verdicts.len(), 1);
        assert_eq!(state.reviewer_verdicts[0].verdict, "PASS");
        assert_eq!(state.reviewer_verdicts[0].blocking_gaps, 0);
        assert_eq!(
            app.ultraplan_status.as_ref().unwrap().last_verdict,
            state.reviewer_verdicts.last().cloned()
        );
    }

    #[test]
    fn canonical_step_heading_is_accepted_by_reviewer_ingestion() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        let mut review_state = load_run(&session, "run-a");
        review_state.set_plan_artifacts(
            "### Step 1: Do work\n- files: src/lib.rs\n- change: implement\n- verify: cargo test"
                .into(),
            PlanCoverageResult {
                covered: vec!["R1".into()],
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            vec![ExecutionCard {
                step: "P1".into(),
                covers: Some("R1".into()),
                files: vec!["src/lib.rs".into()],
                change: "implement".into(),
                verify: "cargo test".into(),
            }],
        );
        review_state.pending_review_plan_hash = review_state.plan_hash.clone();
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &review_state)
            .unwrap();
        insert_reviewer_snapshot(
            &app,
            "reviewer-step-heading",
            &review_state,
            pass_review(&review_state),
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert_eq!(state.reviewer_verdicts.last().unwrap().verdict, "PASS");
        assert!(!state
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.class == UltraplanDiagnosticClass::ReviewerMalformed));
    }

    #[test]
    fn advisory_only_review_passes_and_cannot_patch_requirements() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        let review_state = load_run(&session, "run-a");
        let mut output = pass_review(&review_state);
        output["verdict"] = serde_json::json!("FAIL");
        output["findings"] = serde_json::json!([{
            "classification": "advisory",
            "code": "naming",
            "message": "A clearer name would help",
            "evidence": ["src/lib.rs:1"]
        }]);
        output["requirements_patch"] = serde_json::json!({
            "base_revision": review_state.ledger_revision,
            "items": [{"id": "R2", "title": "Rename", "reason": "advisory only"}]
        });
        insert_reviewer_snapshot(&app, "reviewer-advisory", &review_state, output);

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert_eq!(state.reviewer_verdicts[0].verdict, "PASS");
        assert_eq!(state.auto_review_passed_hash, state.plan_hash);
        assert!(state
            .requirement_ledger
            .iter()
            .all(|entry| entry.id != "R2"));
        assert!(state
            .structured_review
            .as_ref()
            .is_some_and(|review| review.requirements_patch.is_none()));
    }

    #[test]
    fn blocker_review_stops_at_final_gate() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        let review_state = load_run(&session, "run-a");
        let mut output = pass_review(&review_state);
        output["verdict"] = serde_json::json!("FAIL");
        output["findings"] = serde_json::json!([{
            "classification": "blocker",
            "code": "missing_test",
            "message": "Regression coverage is missing",
            "evidence": ["src/lib.rs:1"]
        }]);
        insert_reviewer_snapshot(&app, "reviewer-blocker", &review_state, output);

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert_eq!(state.stage(), UltraplanStage::FinalGate);
        assert_eq!(state.reviewer_verdicts[0].verdict, "FAIL");
        assert_eq!(state.reviewer_verdicts[0].blocking_gaps, 1);
        assert!(state.auto_review_passed_hash.is_none());
    }

    #[test]
    fn blocker_requirements_patch_is_applied_once_as_one_revision() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        let review_state = load_run(&session, "run-a");
        let mut output = pass_review(&review_state);
        output["verdict"] = serde_json::json!("FAIL");
        output["findings"] = serde_json::json!([{
            "classification": "blocker",
            "code": "missing_acceptance",
            "message": "A rollback acceptance criterion is missing",
            "evidence": []
        }]);
        output["requirements_patch"] = serde_json::json!({
            "base_revision": review_state.ledger_revision,
            "items": [{
                "id": "R2",
                "title": "Rollback is verified",
                "reason": "required by the blocker"
            }]
        });
        insert_reviewer_snapshot(&app, "reviewer-patch", &review_state, output);

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);
        assert!(state
            .requirement_ledger
            .iter()
            .any(|entry| entry.id == "R2"));
        assert!(state.pending_requirements_patch.is_none());
        assert!(state.last_plan_draft.is_none());
        assert_eq!(
            state
                .revision_history
                .iter()
                .filter(|snapshot| {
                    matches!(
                        snapshot.mutation,
                        rebon_types::RunRevisionMutation::ReviewPatchApplied
                    )
                })
                .count(),
            1
        );
    }

    #[test]
    fn duplicate_structured_step_coverage_is_malformed() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        disable_reviewer_retries(&session, "run-a");
        let review_state = load_run(&session, "run-a");
        let mut output = pass_review(&review_state);
        let coverage = output["step_coverage"][0].clone();
        output["step_coverage"] = serde_json::json!([coverage.clone(), coverage]);
        insert_reviewer_snapshot(&app, "reviewer-duplicate", &review_state, output);

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert!(state.diagnostics.iter().any(|diagnostic| {
            diagnostic.class == UltraplanDiagnosticClass::ReviewerMalformed
                && diagnostic.message.contains("2 times")
        }));
    }

    #[test]
    fn research_task_usage_is_persisted_without_a_checkpoint() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        insert_completed_snapshot(
            &app,
            "researcher-1",
            serde_json::json!({
                "ultraplan_role": "researcher",
                "ultraplan_id": "run-a"
            }),
            "evidence",
            None,
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert_eq!(state.budget.research_agents_used, 1);
        // Budget synchronization is bookkeeping, not a milestone checkpoint.
        assert!(state.latest_checkpoint().is_none());
    }

    #[test]
    fn ignores_non_reviewer_and_different_ultraplan_id() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        insert_completed_snapshot(
            &app,
            "agent-1",
            serde_json::json!({ "ultraplan_role": "researcher", "ultraplan_id": "run-a" }),
            "ignored prose",
            None,
        );
        insert_completed_snapshot(
            &app,
            "reviewer-missing-id",
            serde_json::json!({ "ultraplan_role": "reviewer" }),
            "ignored prose",
            None,
        );
        insert_completed_snapshot(
            &app,
            "reviewer-other",
            serde_json::json!({
                "ultraplan_role": "reviewer",
                "ultraplan_id": "run-b"
            }),
            "ignored prose",
            None,
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, "run-a")
                .expect("run state");
        assert!(state.reviewer_verdicts.is_empty());
        assert!(app
            .ultraplan_status
            .as_ref()
            .unwrap()
            .last_verdict
            .is_none());
    }

    #[tokio::test]
    async fn malformed_reviewer_output_retries_once_without_consuming_another_review_slot() {
        let (mut app, mut session, _projects_root) = app_and_session_with_run("run-a");
        session.engine_half.engine = crate::tui::runner::test_support::builtin_test_engine();
        let spawner = Arc::new(RecordingSubAgentSpawner::default());
        session.engine_half.sub_agent_spawner = spawner.clone();
        let mut review_state = load_run(&session, "run-a");
        review_state.budget.adversarial_reviews_used = 1;
        let root = std::fs::canonicalize(&session.projects_root).unwrap();
        let mut capability = CapabilityContext {
            run_id: review_state.run_id.clone(),
            ledger_revision: review_state.ledger_revision,
            requirements_hash: review_state.requirements_hash.clone(),
            session_id: session.session_id.clone(),
            cwd: root.to_string_lossy().to_string(),
            allowed_roots: vec![root.to_string_lossy().to_string()],
            read_allowed: true,
            write_allowed: false,
            shell_allowed: false,
            tool_ids: vec![
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
                "StructuredOutput".into(),
            ],
            network: NetworkCapability::Denied,
            workspace_head: None,
            workspace_dirty: Some(false),
            provider: Some(session.model.provider_name.clone()),
            model: Some(session.model.name.clone()),
            sub_agent_available: true,
            max_research_agents: review_state.budget.max_research_agents,
            research_agents_used: review_state.budget.research_agents_used,
            max_adversarial_reviews: review_state.budget.max_adversarial_reviews,
            adversarial_reviews_used: review_state.budget.adversarial_reviews_used,
            max_tool_error_retries: review_state.budget.max_tool_error_retries,
            capability_hash: String::new(),
        };
        capability.refresh_hash();
        review_state.set_capability_context(capability);
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &review_state)
            .unwrap();
        insert_completed_snapshot(
            &app,
            "reviewer-malformed-first",
            reviewer_metadata(&review_state),
            "VERDICT: PASS",
            None,
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            spawner.spawned.notified(),
        )
        .await
        .expect("review retry should spawn");

        let state = load_run(&session, "run-a");
        assert_eq!(state.stage(), UltraplanStage::AdversarialReview);
        assert_eq!(state.budget.adversarial_reviews_used, 1);
        assert!(state.reviewer_verdicts.is_empty());
        assert!(state.diagnostics.is_empty());
        assert_eq!(
            app.ultraplan_status.as_ref().unwrap().phase,
            UltraplanPhase::Reviewing
        );
        {
            let specs = spawner.specs.lock().unwrap();
            assert_eq!(specs.len(), 1);
            assert_eq!(
                specs[0].metadata["retry_of"].as_str(),
                Some("reviewer-malformed-first")
            );
            assert_eq!(
                specs[0].metadata["ultraplan_retry_attempt"].as_bool(),
                Some(true)
            );
            assert_eq!(specs[0].metadata["retry_attempt"].as_u64(), Some(1));
        }

        let mut retry_metadata = reviewer_metadata(&state);
        retry_metadata["ultraplan_retry_attempt"] = serde_json::json!(true);
        retry_metadata["retry_of"] = serde_json::json!("reviewer-malformed-first");
        retry_metadata["retry_attempt"] = serde_json::json!(1);
        insert_completed_snapshot(
            &app,
            "reviewer-malformed-retry",
            retry_metadata,
            "",
            Some(serde_json::json!({"plan_hash": state.plan_hash})),
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        // The failed review released the reservation: the derived stage is
        // back to drafting; the Degraded escape lives in the gate diagnostics.
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);
        assert!(state.pending_review_plan_hash.is_none());
        assert_eq!(state.budget.adversarial_reviews_used, 1);
        assert_eq!(state.reviewer_verdicts.len(), 1);
        assert_eq!(state.reviewer_verdicts[0].verdict, "UNKNOWN");
        assert!(state
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.class == UltraplanDiagnosticClass::ReviewerMalformed));
        assert_eq!(spawner.specs.lock().unwrap().len(), 1);
    }

    #[test]
    fn prose_only_reviewer_output_records_malformed_diagnostic_without_blocking() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        disable_reviewer_retries(&session, "run-a");
        let review_state = load_run(&session, "run-a");
        insert_completed_snapshot(
            &app,
            "reviewer-1",
            reviewer_metadata(&review_state),
            "VERDICT: PASS",
            None,
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert_eq!(state.reviewer_verdicts.len(), 1);
        assert_eq!(state.reviewer_verdicts[0].verdict, "UNKNOWN");
        assert_eq!(state.reviewer_verdicts[0].blocking_gaps, 0);
        assert!(state
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.class == UltraplanDiagnosticClass::ReviewerMalformed }));
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);
        assert!(state.pending_review_plan_hash.is_none());
        assert_eq!(
            app.ultraplan_status.as_ref().unwrap().phase,
            UltraplanPhase::Synthesizing
        );
    }

    #[test]
    fn failed_reviewer_task_records_unavailable_diagnostic() {
        let (mut app, session, _projects_root) = app_and_session_with_run("run-a");
        disable_reviewer_retries(&session, "run-a");
        let review_state = load_run(&session, "run-a");
        insert_terminal_snapshot(
            &app,
            "reviewer-failed",
            TaskStatus::Failed,
            reviewer_metadata(&review_state),
            Some("provider request failed".into()),
            None,
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let state = load_run(&session, "run-a");
        assert_eq!(state.stage(), UltraplanStage::EvidenceVerify);
        assert_eq!(state.reviewer_verdicts[0].verdict, "UNKNOWN");
        assert!(state.diagnostics.iter().any(|diagnostic| {
            diagnostic.class == UltraplanDiagnosticClass::ReviewerUnavailable
                && diagnostic.message.contains("provider request failed")
        }));
    }

    #[test]
    fn reviewer_progress_is_not_accepted_without_structured_output() {
        let snapshot = TaskSnapshot {
            id: TaskId::new("reviewer-progress"),
            kind: TaskKind::LocalAgent,
            status: TaskStatus::Completed,
            title: "reviewer".into(),
            last_progress: Some("VERDICT: PASS".into()),
            error: None,
            result: Some(serde_json::json!({"final_text": "VERDICT: PASS"})),
            is_backgrounded: true,
            notified: false,
            start_time_ms: 0,
            end_time_ms: Some(1),
            metadata: serde_json::json!({
                "ultraplan_role": "reviewer",
                "ultraplan_id": "run-a"
            }),
            data: TaskData::LocalAgent(LocalAgentData {
                prompt: "review".into(),
                agent_type: "reviewer".into(),
                model: None,
                system: None,
                allowed_tools: None,
                token_count: 0,
                tool_use_count: 0,
                transcript: Vec::new(),
                streaming_text: None,
                pending_messages: Vec::new(),
                retrieved: false,
            }),
        };

        assert!(matches!(
            structured_reviewer_result(&snapshot),
            Err((UltraplanDiagnosticClass::ReviewerMalformed, _))
        ));
    }

    #[test]
    fn missing_run_state_does_not_panic_or_corrupt_status() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let temp = tempfile::TempDir::new().expect("temp dir");
        session.projects_root = temp.path().to_path_buf();
        session.cwd = "missing-run-project".into();
        app.tasks = session.engine_half.tasks.clone();
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: "missing-run".into(),
            phase: UltraplanPhase::Reviewing,
            task_title: "task".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: None,
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        insert_completed_snapshot(
            &app,
            "reviewer-1",
            serde_json::json!({
                "ultraplan_role": "reviewer",
                "ultraplan_id": "missing-run"
            }),
            "ignored prose",
            None,
        );

        drain_ultraplan_reviewer_verdicts(&mut app, &session);

        let status = app.ultraplan_status.as_ref().unwrap();
        assert_eq!(status.phase, UltraplanPhase::Reviewing);
        assert!(status.last_verdict.is_none());
    }

    fn app_and_session_with_run(
        run_id: &str,
    ) -> (
        AppState,
        crate::tui::wiring::TuiEngineSession,
        tempfile::TempDir,
    ) {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let temp = tempfile::TempDir::new().expect("temp dir");
        session.projects_root = temp.path().to_path_buf();
        session.cwd = "project".into();
        app.tasks = session.engine_half.tasks.clone();
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: run_id.into(),
            phase: UltraplanPhase::Reviewing,
            task_title: "task".into(),
            started_at_ms: Some(1),
            worker_count: Some(1),
            context: None,
            round: 2,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "task".into(),
            None,
            1,
        );
        state.round = 2;
        state.phase = RunPhase::Reviewing;
        state.record_interview_turn("Scope?".into(), None, "Narrow".into());
        state.set_plan_artifacts(
            "P1. Do work\n- files: src/lib.rs\n- change: implement\n- verify: cargo test".into(),
            PlanCoverageResult {
                covered: vec!["R1".into()],
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            vec![ExecutionCard {
                step: "P1".into(),
                covers: Some("R1".into()),
                files: vec!["src/lib.rs".into()],
                change: "implement".into(),
                verify: "cargo test".into(),
            }],
        );
        // A reviewer task in flight implies the reservation was persisted.
        state.pending_review_plan_hash = state.plan_hash.clone();
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state)
            .expect("save run");
        (app, session, temp)
    }

    fn load_run(session: &crate::tui::wiring::TuiEngineSession, run_id: &str) -> UltraplanRunState {
        rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
            .expect("run state")
    }

    fn disable_reviewer_retries(session: &crate::tui::wiring::TuiEngineSession, run_id: &str) {
        let mut state = load_run(session, run_id);
        state.budget.max_tool_error_retries = 0;
        state.prepare_for_persist();
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
    }

    fn reviewer_metadata(state: &UltraplanRunState) -> serde_json::Value {
        serde_json::json!({
            "ultraplan_role": "reviewer",
            "ultraplan_id": state.run_id.clone(),
            "ultraplan_review_kind": "auto",
            "plan_hash": state.plan_hash.clone(),
            "ultraplan_ledger_revision": state.ledger_revision,
            "ultraplan_requirements_hash": state.requirements_hash.clone(),
        })
    }

    fn pass_review(state: &UltraplanRunState) -> serde_json::Value {
        serde_json::json!({
            "plan_hash": state.plan_hash.clone(),
            "base_revision": state.ledger_revision,
            "verdict": "PASS",
            "findings": [],
            "step_coverage": [{
                "step_id": "P1",
                "ok": true,
                "reason": "implementation and verification are concrete"
            }],
            "requirements_patch": null,
        })
    }

    fn insert_reviewer_snapshot(
        app: &AppState,
        id: &str,
        state: &UltraplanRunState,
        structured_output: serde_json::Value,
    ) {
        insert_completed_snapshot(
            app,
            id,
            reviewer_metadata(state),
            "ignored reviewer prose",
            Some(structured_output),
        );
    }

    fn insert_completed_snapshot(
        app: &AppState,
        id: &str,
        metadata: serde_json::Value,
        final_text: &str,
        structured_output: Option<serde_json::Value>,
    ) {
        insert_terminal_snapshot(
            app,
            id,
            TaskStatus::Completed,
            metadata,
            None,
            Some(serde_json::json!({
                "final_text": final_text,
                "structured_output": structured_output,
            })),
        );
    }

    fn insert_terminal_snapshot(
        app: &AppState,
        id: &str,
        status: TaskStatus,
        metadata: serde_json::Value,
        error: Option<String>,
        result: Option<serde_json::Value>,
    ) {
        app.tasks.insert(
            TaskId::new(id),
            TaskSnapshot {
                id: TaskId::new(id),
                kind: TaskKind::LocalAgent,
                status,
                title: "reviewer".into(),
                last_progress: None,
                error,
                result,
                is_backgrounded: true,
                notified: false,
                start_time_ms: 0,
                end_time_ms: Some(1),
                metadata,
                data: TaskData::LocalAgent(LocalAgentData {
                    prompt: "review".into(),
                    agent_type: "reviewer".into(),
                    model: None,
                    system: None,
                    allowed_tools: None,
                    token_count: 0,
                    tool_use_count: 0,
                    transcript: Vec::new(),
                    streaming_text: None,
                    pending_messages: Vec::new(),
                    retrieved: false,
                }),
            },
            PromptCancel::new(),
        );
    }
}
