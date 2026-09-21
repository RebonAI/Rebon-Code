use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::background::{last_remote_user_turn_uuid, transcript_fingerprint};
use crate::session::transcript_replay::collect_auto_mode_allowed_ids;
use crate::session_shell::handover::{adopt_session_as_owner, release_session_to_owner};
use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

use super::inject_system_message;
use super::remote_background_attachment::sync_remote_tasks;
use super::status_bar::stale_resume_warning;
use super::transcript_replay::replay_transcript_entries_with_agent_tasks;
use super::ultraplan::{match_runtime_resume_session_mode, restore_ultraplan_run_for_runtime};

/// Attach to a session that runs in a worker and mirror it here.
///
/// `handover` says the session is the one already on screen — this terminal
/// just handed it to the worker. Then the transcript view is kept as it is
/// and only the data source changes: resetting and replaying it would paint
/// the same rows a second time, and hold every local row committed since
/// (the handover's own feedback among them) out of the scrollback.
pub(super) fn apply_remote_background_selection(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    target: &crate::background::BackgroundAttachTarget,
    handover: bool,
) -> bool {
    if app.is_loading {
        inject_system_message(
            app,
            "error",
            "Cannot attach a background session while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return false;
    }
    let state = session.server_state.clone();
    if !handover {
        let _ = state.evict_session(&target.session_id);
    }
    let record = match state.load_session(
        &rebon_harness::projects_root(),
        &target.session_id,
        &target.cwd,
        Some(&target.cwd),
        Vec::new(),
    ) {
        Ok(record) => record,
        Err(err) if err.message == format!("Session not found: {}", target.session_id) => state
            .restore_empty_session(
                target.session_id.clone(),
                target.cwd.clone(),
                Vec::new(),
                "default",
            ),
        Err(err) => {
            inject_system_message(
                app,
                "error",
                &format!("Failed to attach background session: {}", err.message),
            );
            return false;
        }
    };

    // A handed-over session keeps the runtime it has: it is this session's
    // already, and it will not run another turn here.
    if !handover {
        if let Err(err) = session.swap_runtime(&target.session_id, &record.cwd, app.is_loading) {
            inject_system_message(
                app,
                "error",
                &format!("Failed to attach background session runtime: {err}"),
            );
            return false;
        }
    }
    // The session is the worker's now: the lock, the MCP servers and the cwd's
    // cron scheduler go together, and go together for a reason worth reading.
    let _ = release_session_to_owner(session);
    app.mid_turn_queued_submit_poller = Some(session.engine_half.runtime.mid_turn_queue.clone());
    session.attached_background_job_id = Some(target.job_id.clone());
    std::env::set_var("REBON_SESSION_ID", &target.session_id);

    if !handover {
        app.reset_transcript_views();
        app.plan_entries.clear();
        app.streaming_token_count = 0;
        app.usage_mut().clear_last_turn();
        app.cwd = target.cwd.clone();
        app.session_title = record.title.clone();
        app.prompt_completion_status = None;
        app.background_agent_tool_tasks.clear();
        app.remote_background_tasks.clear();
        app.live_agent_tool_activity.clear();
        app.live_agent_tool_activity_revision =
            app.live_agent_tool_activity_revision.wrapping_add(1);
    }
    let fingerprint = transcript_fingerprint(&record.loaded_transcript);
    let current_turn_user_uuid = last_remote_user_turn_uuid(&record.loaded_transcript);
    let persisted_transcript_uuids = record
        .loaded_transcript
        .iter()
        .map(|entry| entry.uuid.clone())
        .collect();
    if !handover {
        app.auto_mode_allowed_tool_ids = collect_auto_mode_allowed_ids(&record.loaded_transcript);
        app.background_agent_tool_tasks = replay_transcript_entries_with_agent_tasks(
            &mut app.rebon_tui,
            record.loaded_transcript,
        );
    }
    // Presentation now owns the complete replay. Do not retain a second raw
    // copy in ACP while remotely attached.
    let _ = session
        .engine_half
        .handler
        .state()
        .release_transcript_residency(&target.session_id);

    let store = crate::background::cli_default_store();
    sync_remote_tasks(app, &store, &target.job_id);
    let event_count = store
        .read_state(&target.job_id)
        .map(|state| state.outcome.event_count)
        .unwrap_or(0);
    // No endpoint means no worker to follow: the session is shown as the
    // job's, parked, until a prompt gives the job a worker.
    let mut remote = match target.remote_endpoint.clone() {
        Some(endpoint) => crate::background::RemoteBackgroundAttachment::new(
            target.job_id.clone(),
            target.session_id.clone(),
            target.cwd.clone(),
            target.status,
            event_count,
            endpoint,
        ),
        None => crate::background::RemoteBackgroundAttachment::without_worker(
            target.job_id.clone(),
            target.session_id.clone(),
            target.cwd.clone(),
            target.status,
            event_count,
        ),
    };
    remote.transcript_fingerprint = fingerprint;
    remote.persisted_transcript_uuids = persisted_transcript_uuids;
    remote.current_turn_user_uuid = current_turn_user_uuid;
    // The first gated event read scans history to locate the final queued-user
    // boundary, restore display snapshots, and rebuild only that turn's live
    // overlay. A missing boundary degrades to transcript-only rendering.
    session.remote_background_attachment = Some(remote);
    app.follow_transcript_tail = true;
    true
}

pub(super) fn apply_background_attach_target(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    target: &crate::background::BackgroundAttachTarget,
) -> bool {
    apply_background_attach_target_with_store(app, session, target)
}

/// The session on screen is `job_id`'s, and the job has no worker.
///
/// What a handover that ended without a worker, or `/stop` mid-handover,
/// leaves: the rows stay where they are, the lock and the MCP servers go
/// the way they go for any mirror, and the session waits — parked — for the
/// prompt that gives the job a worker. Returns whether it could be parked;
/// a session whose record cannot be read stays as it was.
pub(super) fn park_session_on_job(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    job_id: &str,
    status: crate::background::BackgroundJobStatus,
) -> bool {
    let target = crate::background::BackgroundAttachTarget {
        overrides: crate::rebon_config::RuntimeOverride::default(),
        cwd: session.cwd.clone(),
        session_id: session.session_id.clone(),
        job_id: job_id.to_string(),
        name: String::new(),
        status,
        summary: None,
        mode: crate::background::BackgroundAttachMode::WorkerStarting,
        remote_endpoint: None,
    };
    apply_handover_attach_target(app, session, &target)
}

/// Finish a handover: the session on screen now runs in `target`'s worker,
/// and this terminal becomes its mirror without redrawing it.
pub(super) fn apply_handover_attach_target(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    target: &crate::background::BackgroundAttachTarget,
) -> bool {
    if !apply_remote_background_selection(app, session, target, true) {
        return false;
    }
    session.attached_background_job_id = Some(target.job_id.clone());
    true
}

fn apply_background_attach_target_with_store(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    target: &crate::background::BackgroundAttachTarget,
) -> bool {
    let attached = match target.mode {
        crate::background::BackgroundAttachMode::RemoteProxy => {
            apply_remote_background_selection(app, session, target, false)
        }
        // No worker to mirror yet: one has been queued for the job. Wait
        // for it to publish an endpoint and mirror it then. The session is
        // never resumed in this process.
        crate::background::BackgroundAttachMode::WorkerStarting => {
            session.pending_hosted_session = Some(
                crate::background::PendingHostedSession::reattach(target.job_id.clone(), false),
            );
            super::inject_local_command_feedback(
                app,
                "attach",
                &format!(
                    "Starting a worker for {} — attaching once it is up…",
                    target.job_id
                ),
            );
            app.follow_transcript_tail = true;
            true
        }
    };
    if attached {
        session.attached_background_job_id = Some(target.job_id.clone());
    }
    attached
}

/// Why a session could not be opened in this process, and where it is.
///
/// The lock only says "held". When a job names the session and has a
/// worker up, that worker is the holder and `rebon attach` is the door — so
/// the refusal names the job instead of "another Rebon instance", which is
/// what a `--local --resume` of a hosted session runs into.
pub(super) fn active_session_refusal(session_id: &str) -> String {
    match crate::background::job_for_session(session_id) {
        Some(job) if job.process.pid.is_some() => format!(
            "Cannot resume session {session_id} in this process: worker {} is hosting it. `rebon attach {}` mirrors it, or resume without --local.",
            job.identity.job_id, job.identity.job_id
        ),
        _ => format!(
            "Cannot resume session {session_id}: it is still active in another Rebon instance."
        ),
    }
}

pub(super) struct ResumePrepareContext {
    pub projects_root: PathBuf,
    pub target_cwd: String,
    pub server_state: std::sync::Arc<rebon_acp::ServerState>,
    pub resume_replay: rebon_core::query::ResumeReplayHandle,
    pub auto_compact_threshold: u32,
}

const RESUME_MODE_CHOICE_AGE: Duration = Duration::from_secs(12 * 60 * 60);

fn requires_resume_mode_choice(
    last_activity_at: SystemTime,
    replayed_tokens: u32,
    auto_compact_threshold: u32,
    now: SystemTime,
) -> bool {
    let is_stale = now
        .duration_since(last_activity_at)
        .is_ok_and(|age| age >= RESUME_MODE_CHOICE_AGE);
    is_stale || replayed_tokens >= auto_compact_threshold
}

pub(super) struct PreparedResume {
    resume_id: String,
    projects_root: PathBuf,
    transcript_cwd: String,
    target_cwd: String,
    state: std::sync::Arc<rebon_acp::ServerState>,
    record: Option<rebon_acp::SessionRecord>,
    evicted_session: Option<rebon_acp::SessionRecord>,
    source_lock: Option<rebon_session::SessionActiveLock>,
    target_lock: Option<rebon_session::SessionActiveLock>,
    summary: Option<rebon_core::query::PreparedResumeSummary>,
    mode: crate::tui::resume_dialog::ResumeMode,
    requires_mode_choice: bool,
    existing_permission_mode: Option<String>,
    resume_replay: rebon_core::query::ResumeReplayHandle,
    committed: bool,
}

impl PreparedResume {
    pub(super) fn requires_mode_choice(&self) -> bool {
        self.requires_mode_choice
    }

    fn rollback(&mut self) -> Option<String> {
        let _ = self.state.evict_session(&self.resume_id);
        let migrating = !rebon_session::same_cwd(&self.transcript_cwd, &self.target_cwd);
        let move_error = if migrating {
            rebon_session::move_session_to_cwd(
                &self.state_projects_root(),
                &self.target_cwd,
                &self.transcript_cwd,
                &self.resume_id,
            )
            .err()
            .map(|err| err.to_string())
        } else {
            None
        };
        if let Some(mut evicted) = self.evicted_session.take() {
            evicted.cwd = if migrating && move_error.is_none() {
                self.transcript_cwd.clone()
            } else {
                self.target_cwd.clone()
            };
            self.state.restore_session(evicted);
        }
        move_error
    }

    fn state_projects_root(&self) -> PathBuf {
        self.projects_root.clone()
    }
}

impl Drop for PreparedResume {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(error) = self.rollback() {
                tracing::error!(
                    session_id = %self.resume_id,
                    error = %error,
                    "failed to roll back cancelled prepared resume"
                );
            }
        }
    }
}

pub(super) fn prepare_interactive_resume(
    context: ResumePrepareContext,
    entry: crate::session::resume_listing::SessionEntry,
    mode: crate::tui::resume_dialog::ResumeMode,
    handle: tokio::runtime::Handle,
) -> Result<PreparedResume, String> {
    let resume_id = entry.session_id;
    let transcript_cwd = entry.transcript_cwd;
    let source_lock = rebon_session::try_acquire_session_active_lock(
        &context.projects_root,
        &transcript_cwd,
        &resume_id,
    )
    .map_err(|err| format!("Could not acquire session lock: {err}"))?
    .ok_or_else(|| active_session_refusal(&resume_id))?;

    let migrating = !rebon_session::same_cwd(&transcript_cwd, &context.target_cwd);
    let target_lock = if migrating {
        Some(
            rebon_session::try_acquire_session_active_lock(
                &context.projects_root,
                &context.target_cwd,
                &resume_id,
            )
            .map_err(|err| format!("Could not acquire target session lock: {err}"))?
            .ok_or_else(|| {
                "The selected session target is active in another Rebon instance.".to_string()
            })?,
        )
    } else {
        None
    };

    if migrating {
        rebon_session::move_session_to_cwd(
            &context.projects_root,
            &transcript_cwd,
            &context.target_cwd,
            &resume_id,
        )
        .map_err(|err| format!("Could not move the session transcript: {err}"))?;
    }

    let existing_permission_mode = context
        .server_state
        .get_session(&resume_id)
        .map(|record| record.permission_mode);
    let evicted_session = context.server_state.evict_session(&resume_id);
    let record = match context.server_state.load_session(
        &context.projects_root,
        &resume_id,
        &context.target_cwd,
        Some(&context.target_cwd),
        Vec::new(),
    ) {
        Ok(record) => record,
        Err(err) => {
            let mut prepared = PreparedResume {
                resume_id,
                projects_root: context.projects_root.clone(),
                transcript_cwd,
                target_cwd: context.target_cwd,
                state: context.server_state,
                record: None,
                evicted_session,
                source_lock: Some(source_lock),
                target_lock,
                summary: None,
                mode,
                requires_mode_choice: false,
                existing_permission_mode,
                resume_replay: context.resume_replay,
                committed: false,
            };
            let rollback = prepared.rollback();
            prepared.committed = true;
            let mut message = err.message;
            if let Some(rollback) = rollback {
                message = format!("{message}; failed to restore transcript location: {rollback}");
            }
            return Err(message);
        }
    };

    let replayed_tokens =
        rebon_core::query::estimate_transcript_input_tokens(&record.loaded_transcript);
    // Cache reuse follows recent transcript activity, not when the session began.
    let last_activity_at = rebon_session::transcript_file_path(
        &context.projects_root,
        &context.target_cwd,
        &resume_id,
    )
    .metadata()
    .and_then(|metadata| metadata.modified())
    .unwrap_or(record.created_at);
    let requires_mode_choice = requires_resume_mode_choice(
        last_activity_at,
        replayed_tokens,
        context.auto_compact_threshold,
        SystemTime::now(),
    );

    let mut prepared = PreparedResume {
        resume_id,
        projects_root: context.projects_root,
        transcript_cwd,
        target_cwd: context.target_cwd,
        state: context.server_state,
        record: Some(record),
        evicted_session,
        source_lock: Some(source_lock),
        target_lock,
        summary: None,
        mode,
        requires_mode_choice,
        existing_permission_mode,
        resume_replay: context.resume_replay,
        committed: false,
    };
    if mode == crate::tui::resume_dialog::ResumeMode::Summary {
        let entries = &prepared
            .record
            .as_ref()
            .expect("prepared resume record missing")
            .loaded_transcript;
        match handle.block_on(prepared.resume_replay.prepare_summary(entries)) {
            Ok(summary) => prepared.summary = Some(summary),
            Err(error) => {
                let rollback = prepared.rollback();
                prepared.committed = true;
                return Err(match rollback {
                    Some(rollback) => {
                        format!("{error}; failed to restore transcript location: {rollback}")
                    }
                    None => error,
                });
            }
        }
    }
    Ok(prepared)
}

pub(super) fn prepare_resumed_session_summary(
    projects_root: PathBuf,
    cwd: String,
    session_id: String,
    resume_replay: rebon_core::query::ResumeReplayHandle,
    handle: tokio::runtime::Handle,
) -> Result<rebon_core::query::PreparedResumeSummary, String> {
    let path = rebon_session::transcript_file_path(&projects_root, &cwd, &session_id);
    let loaded = rebon_session::load_transcript_from_file(&path)
        .map_err(|err| format!("Could not read the resumed transcript: {err}"))?
        .ok_or_else(|| "The resumed transcript no longer exists.".to_string())?;
    handle.block_on(resume_replay.prepare_summary(&loaded.messages))
}

/// What a resumed session puts on screen once its runtime is in place.
///
/// The one landing: reset every view the previous session owned, replay the
/// transcript, restore an active ultraplan run if there is one, and say the
/// two things a resume can have to say about the session it just opened. It
/// was written out twice for a while — once here for the prepared resume
/// committed from the picker, once in an in-process resume that did its own
/// locking — and the two copies drifted before the second was deleted for
/// having had no caller outside `cfg(test)` since the async resume chooser
/// landed.
///
/// Adopting the resumed directory as the screen's and releasing the ACP raw
/// transcript stay at the call site: they belong to committing a resume, not
/// to drawing one. Neither may be dropped. `app.cwd` is what resolves an `@`
/// mention into a file and what a tool's relative path is joined onto, and
/// nothing else in the TUI ever lets the raw copy go, because the idle sweep
/// that would otherwise reclaim it runs only under `serve`.
struct ResumedLanding<'a> {
    record: rebon_acp::SessionRecord,
    projects_root: &'a std::path::Path,
    storage_cwd: &'a str,
    resume_id: &'a str,
    mode_warning: Option<String>,
    stale_warning: Option<String>,
}

/// Returns how many transcript entries were replayed, for the caller's log.
fn land_resumed_session(
    app: &mut AppState,
    session: &TuiEngineSession,
    landing: ResumedLanding<'_>,
) -> usize {
    let ResumedLanding {
        record,
        projects_root,
        storage_cwd,
        resume_id,
        mode_warning,
        stale_warning,
    } = landing;

    app.reset_transcript_views();
    app.plan_entries.clear();
    app.streaming_token_count = 0;
    app.usage_mut().clear_last_turn();
    app.session_title = record.title.clone();
    app.prompt_completion_status = None;
    if app.ui_mode == crate::ui_config::UiMode::Inline {
        app.pending_inline_viewport_reset = true;
        app.pending_inline_startup_banner = false;
    }
    app.background_agent_tool_tasks.clear();
    app.remote_background_tasks.clear();
    app.live_agent_tool_activity.clear();
    app.live_agent_tool_activity_revision = app.live_agent_tool_activity_revision.wrapping_add(1);
    let count = record.loaded_transcript.len();
    app.auto_mode_allowed_tool_ids = collect_auto_mode_allowed_ids(&record.loaded_transcript);
    app.background_agent_tool_tasks =
        replay_transcript_entries_with_agent_tasks(&mut app.rebon_tui, record.loaded_transcript);

    if let Some(mut run_state) =
        rebon_session::latest_active_run_for_session(projects_root, storage_cwd, resume_id)
    {
        let outcome = restore_ultraplan_run_for_runtime(app, &mut run_state);
        run_state.prepare_for_persist();
        let preflight_error =
            crate::session::ultraplan_preflight::preflight_ultraplan_run(session, &mut run_state)
                .err();
        if let Err(err) = rebon_session::save_ultraplan_run(projects_root, storage_cwd, &run_state)
        {
            tracing::warn!(
                error = %err,
                run_id = %run_state.run_id,
                "failed to persist ultraplan run during resume"
            );
        }
        if let Some(diagnostic) = preflight_error {
            inject_system_message(
                app,
                "warning",
                &format!(
                    "Restored ultraplan run with degraded parent-session fallback: {diagnostic}"
                ),
            );
        }
        if outcome.restored {
            inject_system_message(
                app,
                "info",
                &format!(
                    "Restored active ultraplan run {} (phase {:?}, round {}).",
                    run_state.run_id, run_state.phase, run_state.round
                ),
            );
        }
    }
    if let Some(warning) = mode_warning {
        inject_system_message(app, "info", &warning);
    }
    if let Some(warning) = stale_warning {
        inject_system_message(app, "info", &warning);
    }
    count
}

pub(super) fn commit_prepared_resume(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    mut prepared: PreparedResume,
) -> bool {
    if app.is_loading {
        inject_system_message(
            app,
            "error",
            "Cannot resume another session while a prompt is running.",
        );
        app.follow_transcript_tail = true;
        return false;
    }
    let record = prepared
        .record
        .take()
        .expect("prepared resume record missing at commit");
    let resume_id = prepared.resume_id.clone();
    let storage_cwd = record.cwd.clone();
    let active_lock = if rebon_session::same_cwd(&prepared.transcript_cwd, &prepared.target_cwd) {
        prepared.source_lock.take()
    } else {
        prepared.target_lock.take()
    };

    let previous_session_id = session.session_id.clone();
    if let Err(err) = session.swap_runtime(&resume_id, &storage_cwd, app.is_loading) {
        inject_system_message(
            app,
            "error",
            &format!("Failed to resume session: could not build session runtime: {err}"),
        );
        return false;
    }
    app.mid_turn_queued_submit_poller = Some(session.engine_half.runtime.mid_turn_queue.clone());
    if previous_session_id != resume_id {
        session
            .engine_half
            .resume_replay()
            .clear_summary(&previous_session_id);
    }
    match prepared.mode {
        crate::tui::resume_dialog::ResumeMode::Summary => {
            prepared.resume_replay.install_summary(
                resume_id.clone(),
                prepared
                    .summary
                    .take()
                    .expect("summary mode committed without prepared summary"),
            );
        }
        crate::tui::resume_dialog::ResumeMode::FullHistory => {
            prepared.resume_replay.clear_summary(&resume_id);
        }
    }

    session.remote_background_attachment = None;
    if previous_session_id != resume_id {
        session
            .engine_half
            .tasks
            .close_owner_session(&previous_session_id);
    }
    adopt_session_as_owner(session, active_lock);
    std::env::set_var("REBON_SESSION_ID", &resume_id);
    sync_resumed_permission_mode(
        app,
        prepared.state.as_ref(),
        &resume_id,
        prepared.existing_permission_mode.as_deref(),
        crate::rebon_config::saved_default_permission_mode()
            .unwrap_or(rebon_permissions::PermissionMode::Default),
    );
    let mode_warning = match_runtime_resume_session_mode(app, session, record.mode.as_deref());
    let stale_warning = stale_resume_warning(
        record.created_at,
        Some(session.model.prune_level.budget.last_input_tokens()),
    );

    // Only this path adopts the resumed session's directory as the screen's.
    app.cwd = storage_cwd.clone();
    let projects_root = prepared.state_projects_root();
    let count = land_resumed_session(
        app,
        session,
        ResumedLanding {
            record,
            projects_root: &projects_root,
            storage_cwd: &storage_cwd,
            resume_id: &resume_id,
            mode_warning,
            stale_warning,
        },
    );
    // And only this path lets the ACP raw copy go: presentation now owns the
    // complete replay.
    let _ = prepared.state.release_transcript_residency(&resume_id);
    app.follow_transcript_tail = true;
    prepared.committed = true;
    tracing::info!(
        session_id = %resume_id,
        entries = count,
        cwd = %session.cwd,
        mode = ?prepared.mode,
        "rebon: committed prepared resume"
    );
    true
}

fn sync_resumed_permission_mode(
    app: &mut AppState,
    state: &rebon_acp::ServerState,
    session_id: &str,
    existing_permission_mode: Option<&str>,
    default_permission_mode: rebon_permissions::PermissionMode,
) {
    let mode = existing_permission_mode
        .map(rebon_permissions::PermissionMode::from_wire)
        .unwrap_or(default_permission_mode);
    let _ = state.set_permission_mode(session_id, mode.as_wire());
    app.set_permission_mode(mode);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_permissions::PermissionMode;
    use std::path::Path;

    /// A handover mirrors the session already on screen: the rows stay
    /// where they are, the servers are let go, and the attachment knows it
    /// came from this terminal.
    #[test]
    fn a_handover_keeps_the_transcript_view_and_lets_the_mcp_servers_go() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let target_dir = tempfile::tempdir().unwrap();
        let record = session
            .engine_half
            .handler
            .state()
            .clone()
            .create_session(target_dir.path().to_string_lossy().to_string(), Vec::new());
        session.session_id = record.id.clone();
        session.cwd = record.cwd.clone();
        inject_system_message(&mut app, "local_command", "already on screen");
        let rows_before = app.rebon_tui.transcript.rows().len();
        assert!(session.engine_half.mcp.is_some());

        let target = crate::background::BackgroundAttachTarget {
            overrides: crate::rebon_config::RuntimeOverride::default(),
            cwd: record.cwd.clone(),
            session_id: record.id.clone(),
            job_id: "bg-handover".into(),
            name: "handover".into(),
            status: crate::background::BackgroundJobStatus::Idle,
            summary: None,
            mode: crate::background::BackgroundAttachMode::RemoteProxy,
            remote_endpoint: Some(crate::background::BackgroundIpcEndpoint {
                pid: 1,
                port: 1,
                token: "handover-token".into(),
            }),
        };

        assert!(apply_handover_attach_target(
            &mut app,
            &mut session,
            &target
        ));

        assert_eq!(
            app.rebon_tui.transcript.rows().len(),
            rows_before,
            "the view is not reset and replayed"
        );
        assert!(session.remote_background_attachment.is_some(), "mirrored");
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some("bg-handover")
        );
        assert!(
            session.engine_half.mcp.is_none(),
            "a mirror hosts no servers"
        );
        assert!(session.session_active_lock.is_none());
    }

    #[test]
    fn active_prompt_blocks_remote_attachment_replacement() {
        let mut app = AppState::new();
        app.is_loading = true;
        let mut session = super::super::test_support::make_test_tui_session();
        let before_session_id = session.session_id.clone();
        let before_runtime = session.engine_half.runtime.clone();
        let target = crate::background::BackgroundAttachTarget {
            overrides: crate::rebon_config::RuntimeOverride::default(),
            cwd: "/other-cwd".into(),
            session_id: "other-session".into(),
            job_id: "other-job".into(),
            name: "Other job".into(),
            status: crate::background::BackgroundJobStatus::Running,
            summary: None,
            mode: crate::background::BackgroundAttachMode::WorkerStarting,
            remote_endpoint: None,
        };

        assert!(!apply_remote_background_selection(
            &mut app,
            &mut session,
            &target,
            false,
        ));
        assert_eq!(session.session_id, before_session_id);
        assert!(std::sync::Arc::ptr_eq(
            &session.engine_half.runtime,
            &before_runtime
        ));
    }

    /// The prepare takes the source lock before it touches the store, so a
    /// session someone else holds is refused with its record still loaded.
    #[test]
    fn a_held_active_lock_refuses_the_prepare_without_evicting_the_loaded_session() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = super::super::test_support::make_test_tui_session();
        let target = session
            .server_state
            .create_session("locked-source".into(), Vec::new());
        let _external_lock =
            rebon_session::try_acquire_session_active_lock(root.path(), &target.cwd, &target.id)
                .unwrap()
                .expect("acquire competing source lock");

        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: target.cwd.clone(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(&target.id, &target.cwd),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        );
        let error = match prepared {
            Ok(_) => panic!("the prepare should have been refused"),
            Err(error) => error,
        };

        assert!(error.contains(&target.id), "{error}");
        assert!(session.server_state.get_session(&target.id).is_some());
    }

    /// A transcript that is not on disk fails the load after the eviction, so
    /// the record has to come back where it was.
    #[test]
    fn a_failed_disk_load_restores_the_evicted_session() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = super::super::test_support::make_test_tui_session();
        let target = session
            .server_state
            .create_session("missing-transcript".into(), Vec::new());

        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: target.cwd.clone(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(&target.id, &target.cwd),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        );
        let error = match prepared {
            Ok(_) => panic!("the prepare should have been refused"),
            Err(error) => error,
        };

        assert!(!error.is_empty());
        assert_eq!(
            session
                .server_state
                .get_session(&target.id)
                .map(|record| record.cwd),
            Some(target.cwd)
        );
    }

    #[test]
    fn disk_resume_uses_persisted_default_instead_of_current_plan() {
        let state = rebon_acp::ServerState::new();
        let record = state.create_session_with_permission_mode("cwd".into(), Vec::new(), "default");
        let mut app = AppState::default();
        app.set_permission_mode(PermissionMode::Plan);

        sync_resumed_permission_mode(&mut app, &state, &record.id, None, PermissionMode::Auto);

        assert_eq!(app.permission_mode, PermissionMode::Auto);
        assert_eq!(
            *app.permission_mode_cell.lock().expect("permission cell"),
            PermissionMode::Auto
        );
        assert_eq!(
            state.get_session(&record.id).unwrap().permission_mode,
            "auto"
        );
    }

    fn write_resume_transcript(
        projects_root: &Path,
        cwd: &str,
        session_id: &str,
        messages: &[(&str, &str, Option<&str>, &str)],
    ) {
        let path = rebon_session::ensure_session_file_path(projects_root, cwd, session_id).unwrap();
        let mut jsonl = String::new();
        for (entry_type, uuid, parent_uuid, text) in messages {
            let mut value = serde_json::json!({
                "type": entry_type,
                "uuid": uuid,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {
                    "role": entry_type,
                    "content": [{"type": "text", "text": text}],
                },
            });
            if let Some(parent_uuid) = parent_uuid {
                value["parentUuid"] = serde_json::Value::String((*parent_uuid).to_string());
            }
            jsonl.push_str(&value.to_string());
            jsonl.push('\n');
        }
        std::fs::write(path, jsonl).unwrap();
    }

    fn resume_entry(session_id: &str, cwd: &str) -> crate::session::resume_listing::SessionEntry {
        crate::session::resume_listing::SessionEntry {
            session_id: session_id.to_string(),
            transcript_cwd: cwd.to_string(),
            title: "resume target".to_string(),
            created_at_ms: 1,
            jsonl_bytes: Some(1),
            joinable: false,
        }
    }

    #[test]
    fn prepared_full_history_commit_switches_session_atomically() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut session = super::super::test_support::make_test_tui_session();
        session.projects_root = root.path().to_path_buf();
        session.cwd = "/target".to_string();
        let resume_id = "prepared-full";
        write_resume_transcript(
            root.path(),
            "/target",
            resume_id,
            &[
                ("user", "u1", None, "hello"),
                ("assistant", "a1", Some("u1"), "world"),
            ],
        );
        let context = ResumePrepareContext {
            projects_root: root.path().to_path_buf(),
            target_cwd: session.cwd.clone(),
            server_state: session.server_state.clone(),
            resume_replay: session.engine_half.resume_replay(),
            auto_compact_threshold: u32::MAX,
        };
        let prepared = prepare_interactive_resume(
            context,
            resume_entry(resume_id, "/target"),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        )
        .unwrap();
        let mut app = AppState::default();

        assert!(commit_prepared_resume(&mut app, &mut session, prepared));
        assert_eq!(session.session_id, resume_id);
        assert!(session.session_active_lock.is_some());
        assert!(!app.rebon_tui.transcript.rows().is_empty());
    }

    /// The commit is the only place a resume can still be refused, and a
    /// running prompt is what refuses it.
    #[test]
    fn active_prompt_blocks_committing_a_prepared_resume() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut session = super::super::test_support::make_test_tui_session();
        session.projects_root = root.path().to_path_buf();
        session.cwd = "/target".to_string();
        let resume_id = "prepared-while-loading";
        write_resume_transcript(
            root.path(),
            "/target",
            resume_id,
            &[("user", "u1", None, "hello")],
        );
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: session.cwd.clone(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, "/target"),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        )
        .unwrap();
        let mut app = AppState::default();
        app.is_loading = true;
        let before_session_id = session.session_id.clone();
        let before_runtime = session.engine_half.runtime.clone();

        assert!(!commit_prepared_resume(&mut app, &mut session, prepared));

        assert_eq!(session.session_id, before_session_id);
        assert!(std::sync::Arc::ptr_eq(
            &session.engine_half.runtime,
            &before_runtime
        ));
    }

    #[test]
    fn committing_a_prepared_resume_replaces_the_previous_session_title() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut session = super::super::test_support::make_test_tui_session();
        session.projects_root = root.path().to_path_buf();
        session.cwd = "/target".to_string();
        let resume_id = "prepared-title";
        write_resume_transcript(
            root.path(),
            "/target",
            resume_id,
            &[("user", "u1", None, "hello")],
        );
        rebon_session::session_storage::save_session_title(
            root.path(),
            "/target",
            resume_id,
            "Target conversation",
        )
        .unwrap();
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: session.cwd.clone(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, "/target"),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        )
        .unwrap();
        let mut app = AppState::default();
        app.session_title = Some("Previous conversation".into());

        assert!(commit_prepared_resume(&mut app, &mut session, prepared));

        assert_eq!(app.session_title.as_deref(), Some("Target conversation"));
    }

    #[test]
    fn committing_a_prepared_resume_clears_the_title_when_the_target_has_none() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut session = super::super::test_support::make_test_tui_session();
        session.projects_root = root.path().to_path_buf();
        session.cwd = "/target".to_string();
        let resume_id = "prepared-untitled";
        write_resume_transcript(
            root.path(),
            "/target",
            resume_id,
            &[("user", "u1", None, "hello")],
        );
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: session.cwd.clone(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, "/target"),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        )
        .unwrap();
        let mut app = AppState::default();
        app.session_title = Some("Previous conversation".into());

        assert!(commit_prepared_resume(&mut app, &mut session, prepared));

        assert!(app.session_title.is_none());
    }

    #[test]
    fn dropping_prepared_resume_releases_lock_and_restores_state() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = super::super::test_support::make_test_tui_session();
        let resume_id = "prepared-cancel";
        write_resume_transcript(
            root.path(),
            "/target",
            resume_id,
            &[("user", "u1", None, "hello")],
        );
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: "/target".to_string(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, "/target"),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        )
        .unwrap();
        assert!(rebon_session::is_session_active(
            root.path(),
            "/target",
            resume_id
        ));

        drop(prepared);

        assert!(!rebon_session::is_session_active(
            root.path(),
            "/target",
            resume_id
        ));
        assert!(rebon_session::transcript_file_path(root.path(), "/target", resume_id).is_file());
    }

    #[test]
    fn committing_a_cross_cwd_resume_rebinds_the_complete_future_runtime() {
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut session = super::super::test_support::make_test_tui_session();
        session.projects_root = root.path().to_path_buf();
        session.set_test_cwd("/original");
        let old = session.engine_half.runtime.clone();
        let resume_id = "prepared-cross-cwd";
        write_resume_transcript(
            root.path(),
            "/source",
            resume_id,
            &[("user", "u1", None, "hello")],
        );
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: "/target".to_string(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, "/source"),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            tokio_runtime.handle().clone(),
        )
        .unwrap();
        let mut app = AppState::default();

        assert!(commit_prepared_resume(&mut app, &mut session, prepared));

        let current = session.engine_half.runtime.clone();
        assert_eq!(current.session_id, resume_id);
        assert_eq!(current.cwd, "/target");
        assert_eq!(current.projects_root, root.path());
        assert_eq!(old.cwd, "/original");
        assert_ne!(old.session_id, current.session_id);
        assert_eq!(
            current.session_agents.session_binding(),
            (root.path(), "/target", resume_id)
        );
        let hook = current.policy.context().clone();
        assert_eq!(hook.session_id, resume_id);
        assert_eq!(hook.cwd, "/target");
        assert_eq!(
            Path::new(&hook.transcript_path),
            rebon_session::transcript_file_path(root.path(), "/target", resume_id)
        );
        assert_eq!(
            current.file_history_tracker.store().file_history_dir(),
            rebon_session::FileHistoryStore::new(root.path(), "/target", resume_id)
                .file_history_dir()
        );
        let skills = current.skill_state.lock().unwrap();
        assert_eq!(skills.session_binding(), (resume_id, "/target"));
    }

    #[test]
    fn dropping_migrated_prepared_resume_moves_transcript_back() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = super::super::test_support::make_test_tui_session();
        let resume_id = "prepared-migration-cancel";
        write_resume_transcript(
            root.path(),
            "/source",
            resume_id,
            &[("user", "u1", None, "hello")],
        );
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: "/target".to_string(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, "/source"),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            runtime.handle().clone(),
        )
        .unwrap();
        assert!(rebon_session::transcript_file_path(root.path(), "/target", resume_id).is_file());

        drop(prepared);

        assert!(rebon_session::transcript_file_path(root.path(), "/source", resume_id).is_file());
        assert!(!rebon_session::transcript_file_path(root.path(), "/target", resume_id).is_file());
    }

    #[test]
    fn summary_prepare_failure_does_not_fall_back_to_full_history() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = super::super::test_support::make_test_tui_session();
        let resume_id = "prepared-summary-failure";
        write_resume_transcript(
            root.path(),
            "/target",
            resume_id,
            &[
                ("user", "u1", None, "hello"),
                ("assistant", "a1", Some("u1"), "world"),
            ],
        );

        let result = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: "/target".to_string(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, "/target"),
            crate::tui::resume_dialog::ResumeMode::Summary,
            runtime.handle().clone(),
        );

        let error = match result {
            Ok(_) => panic!("summary preparation unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.contains("no compact provider"));
        assert!(!rebon_session::is_session_active(
            root.path(),
            "/target",
            resume_id
        ));
        assert!(rebon_session::transcript_file_path(root.path(), "/target", resume_id).is_file());
    }

    #[test]
    fn resume_mode_choice_is_only_for_inactive_or_overlong_history() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 60 * 60);
        let recently_active = now - Duration::from_secs(11 * 60 * 60 + 59 * 60);
        let inactive = now - Duration::from_secs(12 * 60 * 60);

        assert!(!requires_resume_mode_choice(
            recently_active,
            9_999,
            10_000,
            now
        ));
        assert!(requires_resume_mode_choice(inactive, 1, 10_000, now));
        assert!(requires_resume_mode_choice(
            recently_active,
            10_000,
            10_000,
            now
        ));
    }

    #[test]
    fn in_process_attach_restores_target_sessions_plan_mode() {
        let state = rebon_acp::ServerState::new();
        let record = state.create_session_with_permission_mode("cwd".into(), Vec::new(), "plan");
        let mut app = AppState::default();
        app.set_permission_mode(PermissionMode::Auto);

        sync_resumed_permission_mode(
            &mut app,
            &state,
            &record.id,
            Some(&record.permission_mode),
            PermissionMode::Auto,
        );

        assert_eq!(app.permission_mode, PermissionMode::Plan);
        assert_eq!(
            *app.permission_mode_cell.lock().expect("permission cell"),
            PermissionMode::Plan
        );
        assert_eq!(
            state.get_session(&record.id).unwrap().permission_mode,
            "plan"
        );
    }
    /// A terminal that has been mirroring stopped this directory's cron
    /// scheduler when it gave the session away. Resuming another session takes
    /// the lock back, which makes this process the one that runs it again — and
    /// the scheduled tasks for its directory have to reach the loop that runs
    /// it. Before this was fixed, installing the lock was all
    /// the resume did: the process owned a session and no scheduler, and the
    /// directory's cron went unattended until the terminal was restarted.
    #[test]
    fn resuming_after_mirroring_starts_the_scheduler_again() {
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = tokio_runtime.enter();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().to_string_lossy().into_owned();
        let mut session = super::super::test_support::make_test_tui_session();
        session.projects_root = root.path().to_path_buf();
        session.set_test_cwd(&cwd);

        // This terminal owned a session and ran the directory's scheduler.
        session.session_active_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .expect("a fresh directory is unclaimed");
        session.start_cron_scheduler();
        assert!(session.engine_half.cron_scheduler.is_some());

        // Then it handed that session to a worker and became a mirror.
        let _ = crate::session_shell::handover::release_session_to_owner(&mut session);
        assert!(session.engine_half.cron_scheduler.is_none());

        let resume_id = "resumed-after-mirroring";
        write_resume_transcript(root.path(), &cwd, resume_id, &[("user", "u1", None, "hi")]);
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: cwd.clone(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, &cwd),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            tokio_runtime.handle().clone(),
        )
        .unwrap();
        let mut app = AppState::default();

        assert!(commit_prepared_resume(&mut app, &mut session, prepared));

        assert!(
            session.session_active_lock.is_some(),
            "the resume made this process the owner again"
        );
        assert!(
            session.engine_half.cron_scheduler.is_some(),
            "an owner runs its directory's scheduled tasks; taking the lock \
             without starting the scheduler leaves this cwd's cron unattended"
        );
    }
    /// The scheduler binds its directory when it is built, and resuming into a
    /// different directory changes the session's cwd without rebuilding it.
    /// Before this was fixed the terminal went on holding the *previous*
    /// directory's scheduler lock and firing that directory's tasks, while the
    /// directory it had actually moved to ran none — and starting one was a
    /// no-op, because a scheduler already existed.
    #[test]
    fn resuming_into_another_directory_rebinds_the_scheduler_to_it() {
        fn scheduler_lock_is_free(cwd: &str) -> bool {
            let cron = rebon_tool::cron::tasks::cron_dir(std::path::Path::new(cwd));
            std::fs::create_dir_all(&cron).unwrap();
            matches!(
                rebon_core::cron::try_acquire_scheduler_lock(&cron),
                Ok(Some(_))
            )
        }
        fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !condition() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "gave up waiting for {what}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = tokio_runtime.enter();
        let root = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first = first.path().to_string_lossy().into_owned();
        let second = second.path().to_string_lossy().into_owned();

        let mut session = super::super::test_support::make_test_tui_session();
        session.projects_root = root.path().to_path_buf();
        session.set_test_cwd(&first);
        session.session_active_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .expect("a fresh directory is unclaimed");
        session.start_cron_scheduler();
        wait_until("the first directory's scheduler to take its lock", || {
            !scheduler_lock_is_free(&first)
        });

        let resume_id = "resumed-elsewhere";
        write_resume_transcript(
            root.path(),
            &second,
            resume_id,
            &[("user", "u1", None, "hi")],
        );
        let prepared = prepare_interactive_resume(
            ResumePrepareContext {
                projects_root: root.path().to_path_buf(),
                target_cwd: second.clone(),
                server_state: session.server_state.clone(),
                resume_replay: session.engine_half.resume_replay(),
                auto_compact_threshold: u32::MAX,
            },
            resume_entry(resume_id, &second),
            crate::tui::resume_dialog::ResumeMode::FullHistory,
            tokio_runtime.handle().clone(),
        )
        .unwrap();
        let mut app = AppState::default();

        assert!(commit_prepared_resume(&mut app, &mut session, prepared));
        assert_eq!(session.cwd, second, "the session moved directories");

        wait_until("the resumed directory's scheduler to take its lock", || {
            !scheduler_lock_is_free(&second)
        });
        wait_until(
            "the previous directory's scheduler lock to be released",
            || scheduler_lock_is_free(&first),
        );
    }
}
