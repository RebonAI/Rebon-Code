use std::sync::mpsc::{self, Receiver};

use tokio::runtime::Handle;

use crate::session::resume_listing::SessionEntry;
use crate::tui::app::AppState;
use crate::tui::resume_dialog::{ResumeLoadRequest, ResumePrepareRequest, ResumeSummaryRequest};
use crate::tui::wiring::TuiEngineSession;

use super::resume_selection::{
    apply_background_attach_target, commit_prepared_resume, prepare_interactive_resume,
    prepare_resumed_session_summary, PreparedResume, ResumePrepareContext,
};

pub(super) struct ResumeRuntime {
    load: Option<ResumeLoadWorker>,
    prepare: Option<ResumePrepareWorker>,
    summary: Option<ResumeSummaryWorker>,
}

impl ResumeRuntime {
    pub(super) fn new() -> Self {
        Self {
            load: None,
            prepare: None,
            summary: None,
        }
    }
}

/// One discovery batch on its way from the load worker to the dialog.
struct ResumeLoadChunk {
    entries: Vec<SessionEntry>,
    /// Last message of this stream — no further batches will arrive.
    complete: bool,
}

struct ResumeLoadWorker {
    generation: u64,
    rx: Receiver<Result<ResumeLoadChunk, String>>,
}

struct ResumePrepareWorker {
    generation: u64,
    rx: Receiver<Result<PreparedResume, String>>,
}

struct ResumeSummaryWorker {
    generation: u64,
    session_id: String,
    rx: Receiver<Result<rebon_core::query::PreparedResumeSummary, String>>,
}

pub(super) fn sync_resume_dialog(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    runtime: &mut ResumeRuntime,
) {
    let store = crate::background::cli_default_store();
    sync_resume_dialog_in_store(app, session, handle, runtime, &store, true);
}

/// `start_supervisor` is what actually brings a revived worker up; tests
/// pass `false` and read the job record instead.
pub(super) fn sync_resume_dialog_in_store(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    runtime: &mut ResumeRuntime,
    store: &crate::background::BackgroundStore,
    start_supervisor: bool,
) {
    let Some(dialog) = app.resume_dialog.as_mut() else {
        runtime.load = None;
        runtime.prepare = None;
        runtime.summary = None;
        return;
    };

    if let Some(request) = dialog.maybe_take_load_request() {
        runtime.load = Some(spawn_resume_load_worker(request, session));
    }
    let prepare_request = dialog.maybe_take_prepare_request();
    if let Some(request) = dialog.maybe_take_summary_request() {
        runtime.summary = Some(spawn_resume_summary_worker(
            request,
            session,
            handle.clone(),
        ));
    }

    // Discovery streams batches, so take everything queued this pass —
    // one batch per frame would re-introduce the stall the batching is
    // there to remove.
    while let Some(active) = runtime.load.as_ref() {
        match active.rx.try_recv() {
            Ok(Ok(chunk)) => {
                let complete = chunk.complete;
                dialog.apply_load_chunk(active.generation, chunk.entries, complete);
                if complete {
                    runtime.load = None;
                }
            }
            Ok(Err(message)) => {
                dialog.apply_load_failure(active.generation, message);
                runtime.load = None;
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                dialog.apply_load_failure(
                    active.generation,
                    "Session discovery stopped before returning results.".to_string(),
                );
                runtime.load = None;
            }
        }
    }

    // A session the picker chose is opened where it lives. One a job
    // names is attached — its worker mirrored, or revived and then
    // mirrored — on this thread, since a job without a live worker costs a
    // record write and no network. One no job names is given a job and a
    // worker and mirrored the same way (RFC-0004 §9: hosted is the
    // default, and a session resumed is a session started); nothing is
    // built or locked here. Only `--local` still prepares the transcript in
    // this process, off-thread as before.
    if let Some(request) = prepare_request {
        let job = if session.terminal_startup.local {
            Ok(None)
        } else {
            rebon_session_host::home_job_for_session(store, &request.entry.session_id)
        };
        match job {
            Ok(Some(job)) => {
                attach_instead_of_resume(app, session, store, start_supervisor, request, &job)
            }
            Ok(None) if !session.terminal_startup.local => {
                resume_in_a_worker(app, session, store, start_supervisor, request)
            }
            Ok(None) => {
                runtime.prepare = Some(spawn_resume_prepare_worker(
                    request,
                    session,
                    handle.clone(),
                ));
            }
            Err(err) => {
                if let Some(dialog) = app.resume_dialog.as_mut() {
                    dialog.apply_prepare_failure(
                        request.generation,
                        format!(
                            "Could not check whether the session lives in a background job: {err}"
                        ),
                    );
                }
            }
        }
    }

    let prepare_result = runtime
        .prepare
        .as_ref()
        .and_then(|active| match active.rx.try_recv() {
            Ok(result) => Some((active.generation, result)),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some((
                active.generation,
                Err("Session preparation stopped before returning results.".to_string()),
            )),
        });
    if let Some((generation, result)) = prepare_result {
        runtime.prepare = None;
        match result {
            Ok(prepared) => {
                let requires_mode_choice = prepared.requires_mode_choice();
                commit_prepared_resume(app, session, prepared);
                if requires_mode_choice {
                    if let Some(dialog) = app.resume_dialog.as_mut() {
                        dialog.apply_prepare_success(generation);
                    }
                } else {
                    app.resume_dialog = None;
                }
            }
            Err(message) => {
                if let Some(dialog) = app.resume_dialog.as_mut() {
                    dialog.apply_prepare_failure(generation, message);
                }
            }
        }
    }

    let summary_result = runtime
        .summary
        .as_ref()
        .and_then(|active| match active.rx.try_recv() {
            Ok(result) => Some((active.generation, active.session_id.clone(), result)),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some((
                active.generation,
                active.session_id.clone(),
                Err("Summary generation stopped before returning results.".to_string()),
            )),
        });
    if let Some((generation, session_id, result)) = summary_result {
        runtime.summary = None;
        match result {
            Ok(summary)
                if session.session_id == session_id
                    && app
                        .resume_dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.accepts_summary_success(generation)) =>
            {
                session
                    .engine_half
                    .resume_replay()
                    .install_summary(session_id, summary);
                app.resume_dialog = None;
            }
            Ok(_) => {}
            Err(message) => {
                if let Some(dialog) = app.resume_dialog.as_mut() {
                    dialog.apply_summary_failure(generation, message);
                }
            }
        }
    }
}

/// The chosen session lives in `job`: attach to it the way `rebon attach`
/// would, and close the picker if that took.
///
/// A live worker is mirrored; a job without one — stopped, finished,
/// crashed — is given a worker and mirrored once it is up. The one thing
/// refused is a worker that is alive but not answering: nothing starts a
/// second worker beside a living one, and the picker says so.
fn attach_instead_of_resume(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    store: &crate::background::BackgroundStore,
    start_supervisor: bool,
    request: ResumePrepareRequest,
    job: &crate::background::BackgroundJobState,
) {
    let attached = match crate::background::attach_background_job_in_store_with_supervisor(
        store,
        &job.identity.job_id,
        start_supervisor,
    ) {
        Ok(target) => {
            if apply_background_attach_target(app, session, &target) {
                Ok(())
            } else {
                Err(format!(
                    "Session {} lives in background job {}, and attaching to it did not take.",
                    request.entry.session_id, job.identity.job_id
                ))
            }
        }
        Err(err) => Err(format!(
            "Session {} lives in background job {}: {err}",
            request.entry.session_id, job.identity.job_id
        )),
    };
    match attached {
        Ok(()) => {
            app.resume_dialog = None;
            app.follow_transcript_tail = true;
        }
        Err(message) => {
            if let Some(dialog) = app.resume_dialog.as_mut() {
                dialog.apply_prepare_failure(request.generation, message);
            }
        }
    }
}

/// The chosen session was never hosted: give it a worker and wait to
/// mirror it, as `rebon attach` waits for a replacement worker.
///
/// The placeholder on screen is the job's from here on — typing queues on
/// the job — and is rebuilt from the session's transcript once the worker
/// is up (`Reattach { keep_view: false }`), the same one line that any
/// worker start prints. A session another process still holds open is
/// refused as a local resume would refuse it, naming where it is.
fn resume_in_a_worker(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    store: &crate::background::BackgroundStore,
    start_supervisor: bool,
    request: ResumePrepareRequest,
) {
    let session_id = request.entry.session_id.clone();
    let cwd = request.entry.transcript_cwd.clone();
    let hosted = if rebon_session::is_session_active(&session.projects_root, &cwd, &session_id) {
        Err(super::resume_selection::active_session_refusal(&session_id))
    } else {
        let runtime = crate::session_shell::handover::background_runtime_from_session(
            session,
            session.ui_mode,
            app.effort_level,
            app.permission_mode,
        );
        rebon_session_host::host_existing_session(
            store,
            &session_id,
            &cwd,
            runtime,
            start_supervisor,
            &crate::background::rebon_exe(),
        )
        .map_err(|err| format!("Could not start a worker for session {session_id}: {err:#}"))
    };
    match hosted {
        Ok(job_id) => {
            session.attached_background_job_id = Some(job_id.clone());
            session.pending_hosted_session = Some(
                crate::background::PendingHostedSession::reattach(job_id.clone(), false),
            );
            super::inject_local_command_feedback(
                app,
                "resume",
                &format!("Resuming session {session_id} in worker {job_id}…"),
            );
            app.resume_dialog = None;
            app.follow_transcript_tail = true;
        }
        Err(message) => {
            if let Some(dialog) = app.resume_dialog.as_mut() {
                dialog.apply_prepare_failure(request.generation, message);
            }
        }
    }
}

fn spawn_resume_load_worker(
    request: ResumeLoadRequest,
    session: &TuiEngineSession,
) -> ResumeLoadWorker {
    let projects_root = session.projects_root.clone();
    let cwd = session.cwd.clone();
    let current_session_id = session.session_id.clone();
    let server_state = session.server_state.clone();
    let exact_session_id = request.exact_session_id.clone();
    let generation = request.generation;
    let (tx, rx) = mpsc::channel::<Result<ResumeLoadChunk, String>>();
    std::thread::spawn(move || {
        // Both arms end with one `complete` message; the browse arm also
        // ships every hydrated batch as it lands so the picker can paint
        // its first page while the rest of the history is still being read.
        let tail: Result<Vec<SessionEntry>, String> = match exact_session_id {
            Some(session_id) => crate::session::resume_listing::discover_exact_entry(
                &projects_root,
                &cwd,
                &server_state,
                &session_id,
            ),
            None => {
                let batch_tx = tx.clone();
                // A send failure means the dialog closed; returning false
                // abandons the rest of the scan.
                crate::session::resume_listing::stream_entries(
                    &projects_root,
                    &cwd,
                    &server_state,
                    &current_session_id,
                    |entries| {
                        batch_tx
                            .send(Ok(ResumeLoadChunk {
                                entries,
                                complete: false,
                            }))
                            .is_ok()
                    },
                )
                .map(|()| Vec::new())
            }
        };
        let _ = tx.send(tail.map(|entries| ResumeLoadChunk {
            entries,
            complete: true,
        }));
    });
    ResumeLoadWorker { generation, rx }
}

fn spawn_resume_prepare_worker(
    request: ResumePrepareRequest,
    session: &TuiEngineSession,
    handle: Handle,
) -> ResumePrepareWorker {
    let context = ResumePrepareContext {
        projects_root: session.projects_root.clone(),
        target_cwd: session.cwd.clone(),
        server_state: session.server_state.clone(),
        resume_replay: session.engine_half.resume_replay(),
        auto_compact_threshold: session.model.prune_level.budget.auto_compact_threshold(),
    };
    let generation = request.generation;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = prepare_interactive_resume(context, request.entry, request.mode, handle);
        let _ = tx.send(result);
    });
    ResumePrepareWorker { generation, rx }
}

fn spawn_resume_summary_worker(
    request: ResumeSummaryRequest,
    session: &TuiEngineSession,
    handle: Handle,
) -> ResumeSummaryWorker {
    let projects_root = session.projects_root.clone();
    let cwd = session.cwd.clone();
    let session_id = request.entry.session_id;
    let worker_session_id = session_id.clone();
    let resume_replay = session.engine_half.resume_replay();
    let generation = request.generation;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = prepare_resumed_session_summary(
            projects_root,
            cwd,
            worker_session_id,
            resume_replay,
            handle,
        );
        let _ = tx.send(result);
    });
    ResumeSummaryWorker {
        generation,
        session_id,
        rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::resume_dialog::ResumeDialogState;

    fn runtime_fields() -> crate::background::BackgroundRuntimeFields {
        crate::background::BackgroundRuntimeFields {
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

    fn entry(session_id: &str, cwd: &str) -> SessionEntry {
        SessionEntry {
            session_id: session_id.to_string(),
            transcript_cwd: cwd.to_string(),
            title: "hosted once".to_string(),
            created_at_ms: 1,
            jsonl_bytes: Some(1),
            joinable: false,
        }
    }

    fn transcript_mentions(app: &AppState, needle: &str) -> bool {
        app.rebon_tui.transcript.rows().iter().any(|row| {
            matches!(row, rebon_tui::Message::System(message)
                if message.content.as_deref().is_some_and(|c| c.contains(needle)))
        })
    }

    /// Picking a session a job names goes back to the job: the stopped
    /// worker is given a successor and the terminal waits to mirror it,
    /// instead of taking the session's lock and opening it here. The
    /// picker closes; the session on screen is the job's now.
    #[test]
    fn picking_a_session_that_lives_in_a_job_attaches_instead_of_resuming_locally() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "hosted once".into(),
                std::path::PathBuf::from("."),
                runtime_fields(),
            )
            .unwrap();
        job.process.status = crate::background::BackgroundJobStatus::Stopped;
        job.identity.session_id = Some("sess-lives-in-a-job".into());
        store.write_state(&job).unwrap();
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut dialog = ResumeDialogState::open_exact("sess-lives-in-a-job".into());
        let _ = dialog.maybe_take_load_request();
        dialog.apply_load_chunk(1, vec![entry("sess-lives-in-a-job", &session.cwd)], true);
        app.resume_dialog = Some(dialog);
        let mut runtime = ResumeRuntime::new();

        sync_resume_dialog_in_store(
            &mut app,
            &mut session,
            tokio_runtime.handle(),
            &mut runtime,
            &store,
            false,
        );

        assert!(app.resume_dialog.is_none(), "the picker is done");
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some(job.identity.job_id.as_str())
        );
        assert!(matches!(
            session
                .pending_hosted_session
                .as_ref()
                .map(|pending| pending.kind),
            Some(crate::background::PendingHostedKind::Reattach { keep_view: false })
        ));
        assert!(
            session.session_active_lock.is_none(),
            "the session is not opened in this process"
        );
        assert_eq!(
            store
                .read_state(&job.identity.job_id)
                .unwrap()
                .process
                .status,
            crate::background::BackgroundJobStatus::Queued,
            "the stopped job was given a worker"
        );
        assert!(transcript_mentions(&app, "Starting a worker"));
        assert!(runtime.prepare.is_none(), "nothing was prepared locally");
    }

    /// A living worker that does not answer is never given a sibling: the
    /// picker reports it and stays open, the same refusal `rebon attach`
    /// makes.
    #[test]
    fn a_session_whose_worker_is_alive_but_unreachable_is_refused_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "wedged".into(),
                std::path::PathBuf::from("."),
                runtime_fields(),
            )
            .unwrap();
        job.process.status = crate::background::BackgroundJobStatus::Idle;
        job.identity.session_id = Some("sess-wedged-worker".into());
        job.process.pid = Some(std::process::id());
        job.process.ipc_port = Some(1);
        job.process.ipc_token = Some("unreachable".into());
        store.write_state(&job).unwrap();
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut dialog = ResumeDialogState::open_exact("sess-wedged-worker".into());
        let _ = dialog.maybe_take_load_request();
        dialog.apply_load_chunk(1, vec![entry("sess-wedged-worker", &session.cwd)], true);
        app.resume_dialog = Some(dialog);
        let mut runtime = ResumeRuntime::new();

        sync_resume_dialog_in_store(
            &mut app,
            &mut session,
            tokio_runtime.handle(),
            &mut runtime,
            &store,
            false,
        );

        assert!(app.resume_dialog.is_some(), "the picker stays to say why");
        assert!(session.attached_background_job_id.is_none());
        assert!(session.pending_hosted_session.is_none());
        let loaded = store.read_state(&job.identity.job_id).unwrap();
        assert_eq!(loaded.process.pid, Some(std::process::id()));
        assert_eq!(
            loaded.process.status,
            crate::background::BackgroundJobStatus::Idle
        );
        assert!(runtime.prepare.is_none(), "not resumed locally either");
    }

    /// A session no job names is given one: a `Foreground` job queued
    /// `resume_only` on the session, the terminal waiting to mirror its
    /// worker. Nothing is prepared or locked in this process, and the one
    /// line says where the session is going.
    #[test]
    fn a_session_no_job_names_is_resumed_in_a_worker() {
        let _guard = crate::test_env::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut dialog = ResumeDialogState::open_exact("sess-never-hosted".into());
        let _ = dialog.maybe_take_load_request();
        dialog.apply_load_chunk(1, vec![entry("sess-never-hosted", &session.cwd)], true);
        app.resume_dialog = Some(dialog);
        let mut runtime = ResumeRuntime::new();

        sync_resume_dialog_in_store(
            &mut app,
            &mut session,
            tokio_runtime.handle(),
            &mut runtime,
            &store,
            false,
        );

        assert!(runtime.prepare.is_none(), "nothing was prepared locally");
        assert!(app.resume_dialog.is_none(), "the picker is done");
        let job_id = session
            .attached_background_job_id
            .clone()
            .expect("the session is the new job's");
        let job = store.read_state(&job_id).unwrap();
        assert_eq!(
            job.identity.session_id.as_deref(),
            Some("sess-never-hosted")
        );
        assert_eq!(
            job.lease.placement,
            rebon_session_host::JobPlacement::Foreground
        );
        assert_eq!(
            job.process.status,
            crate::background::BackgroundJobStatus::Queued
        );
        assert!(job.identity.resume_only);
        assert!(matches!(
            session
                .pending_hosted_session
                .as_ref()
                .map(|pending| pending.kind),
            Some(crate::background::PendingHostedKind::Reattach { keep_view: false })
        ));
        assert_ne!(
            session.session_id, "sess-never-hosted",
            "the placeholder stays until the worker is mirrored"
        );
        assert!(!rebon_session::is_session_active(
            &session.projects_root,
            &session.cwd,
            "sess-never-hosted"
        ));
        assert!(transcript_mentions(
            &app,
            "Resuming session sess-never-hosted in worker"
        ));
    }

    /// `--local` is the one way to open a transcript in this process: the
    /// session is prepared here as it always was, and no job is made.
    #[test]
    fn a_local_terminal_still_resumes_in_this_process() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let tokio_runtime = tokio::runtime::Runtime::new().unwrap();
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.terminal_startup.local = true;
        let mut dialog = ResumeDialogState::open_exact("sess-only-local".into());
        let _ = dialog.maybe_take_load_request();
        dialog.apply_load_chunk(1, vec![entry("sess-only-local", &session.cwd)], true);
        app.resume_dialog = Some(dialog);
        let mut runtime = ResumeRuntime::new();

        sync_resume_dialog_in_store(
            &mut app,
            &mut session,
            tokio_runtime.handle(),
            &mut runtime,
            &store,
            false,
        );

        assert!(runtime.prepare.is_some(), "prepared for a local resume");
        assert!(session.attached_background_job_id.is_none());
        assert!(
            store.list_jobs().unwrap().is_empty(),
            "no job was made for it"
        );
    }
}
