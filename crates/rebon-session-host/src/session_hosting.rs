//! Starting a session in a worker: name it, give it a job, spawn the worker.
//!
//! Every endpoint that hosts a session — the terminal's startup
//! route and `--resume`, `rebon serve`, and the Remote Control runner — gives
//! it a worker the same way: the session on disk, a `Foreground` job queued
//! `resume_only` on it, and a worker spawned from the calling process rather
//! than left to the supervisor's tick. None of it takes the session's lock;
//! the worker does when it resumes.
//!
//! What stays with an endpoint is how it resolves
//! the inputs — which store, which projects root, which runtime, which
//! executable.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{
    ensure_supervisor_running, now_ms, spawn_worker_process, BackgroundJobState,
    BackgroundJobStatus, BackgroundRuntimeFields, BackgroundStore, JobPlacement,
};

/// What was started for a brand-new session.
#[derive(Debug, Clone)]
pub struct HostedStartup {
    pub session_id: String,
    pub job_id: String,
}

/// The job name a session gets when it is handed to a worker: the tail of
/// its id, so two sessions of one project read apart in the board.
pub fn session_job_name(session_id: &str) -> String {
    format!(
        "session {}",
        session_id
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    )
}

/// The job a session lives in, if it lives in one.
///
/// A session that has ever been a worker's is named on that worker's job,
/// and resuming such a session means going back to the job — a live worker
/// is mirrored, a missing one is revived — rather than opening the
/// transcript in this process. More than one job can name a session (an
/// attach-here turn later handed over, a respawn); a job with a live owner
/// wins, then the most recently touched. Jobs reserved for removal or
/// already respawned are not the session's home any more.
///
/// Not the same question as [`crate::latest_job_for_session`], which answers
/// "which record last described this session" for carrying its runtime
/// forward and considers every record.
pub fn home_job_for_session(
    store: &BackgroundStore,
    session_id: &str,
) -> anyhow::Result<Option<BackgroundJobState>> {
    let mut candidates = store
        .list_jobs()?
        .into_iter()
        .filter(|job| job.identity.session_id.as_deref() == Some(session_id))
        .filter(|job| !job.process.removal_reserved && job.identity.respawned_job_id.is_none())
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.process
            .pid
            .is_some()
            .cmp(&a.process.pid.is_some())
            .then_with(|| b.process.updated_at_ms.cmp(&a.process.updated_at_ms))
    });
    Ok(candidates.into_iter().next())
}

/// Give a brand-new `session_id` a job to live in, queued for a worker.
///
/// The job is created queued `resume_only`: the worker that picks it up
/// resumes the session and parks on it. Two writes and nothing read — the
/// session is new, so no job can name it yet, and the listing an adoption
/// makes to check is the one cost on the path to the first frame this
/// process does not have to pay. Nothing is spawned here —
/// [`spawn_queued_worker_now`] does that from the calling process, and the
/// supervisor does it for a job left to its tick.
pub fn queue_session_for_worker(
    store: &BackgroundStore,
    session_id: &str,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    name: Option<String>,
    placement: JobPlacement,
) -> anyhow::Result<BackgroundJobState> {
    let job =
        store.create_job_with_name(format!("Continue session {session_id}"), cwd, runtime, name)?;
    let state = store.update_state(&job.identity.job_id, |state| {
        state.lease.placement = placement;
        state.identity.session_id = Some(session_id.to_string());
        state.process.status = BackgroundJobStatus::Queued;
        state.identity.resume_only = true;
        state.process.updated_at_ms = now_ms();
        Ok(state.clone())
    })?;
    store.append_event(
        &job.identity.job_id,
        "session_adopted",
        serde_json::json!({ "sessionId": session_id, "resumed": true }),
    )?;
    Ok(state)
}

/// Spawn the worker for a queued job from *this* process, now.
///
/// Rather than leaving it for the supervisor's next tick: a second of
/// supervisor latency would be a second between the user's first frame and
/// their first token. The supervisor is still made sure of afterwards — it
/// is what reaps, replaces, and eventually collects the worker — but the
/// spawn does not wait for it. Returns the job as recorded after the spawn;
/// a job the spawn found already taken elsewhere is returned as it is, and
/// whoever took it finishes what they started.
///
/// `rebon_exe_path` is the already-resolved executable the supervisor is
/// started from; it is never looked up on `PATH`.
pub fn spawn_queued_worker_now(
    store: &BackgroundStore,
    job_id: &str,
    rebon_exe_path: &Path,
) -> anyhow::Result<BackgroundJobState> {
    // Best effort, and off this thread: without a supervisor the worker
    // still runs, only its exit goes unreaped until one comes along — and
    // starting one is a process spawn the first frame should not wait for.
    // Started before the spawn below, not after: a spawn that fails leaves
    // the job queued for the supervisor, which then has to exist.
    let supervisor_store = store.clone();
    let supervisor_job_id = job_id.to_string();
    let supervisor_exe = rebon_exe_path.to_path_buf();
    std::thread::Builder::new()
        .name("rebon-ensure-supervisor".to_string())
        .spawn(move || {
            if let Err(err) = ensure_supervisor_running(&supervisor_store, &supervisor_exe) {
                tracing::warn!(job_id = %supervisor_job_id, %err, "rebon: could not make sure of a supervisor");
            }
        })
        .ok();
    let mut state = store.read_state(job_id)?;
    spawn_worker_process(store, &mut state)?;
    store.append_event(
        job_id,
        "worker_spawned_by_client",
        serde_json::json!({ "pid": state.process.pid }),
    )?;
    Ok(state)
}

/// Name a brand-new session and start the worker that will host it.
///
/// A few file writes and a `spawn`: the session id, an empty transcript
/// (what lets the worker resume "this session, from the beginning"), a
/// `Foreground` job queued `resume_only`, and the worker process. The
/// worker takes the session's lock when it resumes; this process never
/// does. Fails only when nothing was started — the caller then decides
/// whether to host the session in-process.
///
/// The file writes happen here; the process spawn happens on a thread of
/// its own (the frame and the spawn share t1). A spawn that
/// fails leaves the job queued and admitted to nobody, and the supervisor
/// the same thread makes sure of dispatches it on its next tick.
pub fn start_hosted_session(
    store: &BackgroundStore,
    projects_root: &Path,
    runtime: BackgroundRuntimeFields,
    cwd: &str,
    rebon_exe_path: &Path,
) -> anyhow::Result<HostedStartup> {
    let started = Instant::now();
    let hosted = prepare_hosted_session(store, projects_root, runtime, cwd)?;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        session_id = %hosted.session_id,
        job_id = %hosted.job_id,
        "rebon startup: session host prepared"
    );
    spawn_worker_on_a_thread(
        store.clone(),
        hosted.session_id.clone(),
        hosted.job_id.clone(),
        started,
        rebon_exe_path.to_path_buf(),
    )?;
    Ok(hosted)
}

/// Give an existing session — one no job names yet — a worker to live in,
/// and start it.
///
/// What `--resume` / `--continue` / the picker do for a session that was
/// never hosted, and what `serve` does for a session it named or lost the
/// worker of: a `Foreground` job queued `resume_only` on the session's
/// transcript where it lies, and a worker spawned for it from here. Nothing
/// is built or locked in this process. `spawn` is what actually starts the
/// worker; tests pass `false` and read the record. Returns the job id.
pub fn host_existing_session(
    store: &BackgroundStore,
    session_id: &str,
    cwd: &str,
    runtime: BackgroundRuntimeFields,
    spawn: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<String> {
    let started = Instant::now();
    let job = queue_session_for_worker(
        store,
        session_id,
        PathBuf::from(cwd),
        runtime,
        Some(session_job_name(session_id)),
        JobPlacement::Foreground,
    )?;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        %session_id,
        job_id = %job.identity.job_id,
        "rebon resume: session queued for a worker"
    );
    if spawn {
        spawn_worker_on_a_thread(
            store.clone(),
            session_id.to_string(),
            job.identity.job_id.clone(),
            started,
            rebon_exe_path.to_path_buf(),
        )?;
    }
    Ok(job.identity.job_id)
}

/// Everything but the spawn: the session on disk and the job queued for it.
fn prepare_hosted_session(
    store: &BackgroundStore,
    projects_root: &Path,
    runtime: BackgroundRuntimeFields,
    cwd: &str,
) -> anyhow::Result<HostedStartup> {
    let session_id = rebon_types::new_session_id();
    rebon_session::create_session_on_disk(projects_root, cwd, &session_id)?;
    let job = queue_session_for_worker(
        store,
        &session_id,
        PathBuf::from(cwd),
        runtime,
        Some(session_job_name(&session_id)),
        JobPlacement::Foreground,
    )?;
    Ok(HostedStartup {
        session_id,
        job_id: job.identity.job_id,
    })
}

/// The spawn, off the calling thread. A spawn that fails is logged: the job
/// is queued, and the supervisor [`spawn_queued_worker_now`] makes sure of
/// picks it up on its tick.
fn spawn_worker_on_a_thread(
    store: BackgroundStore,
    session_id: String,
    job_id: String,
    started: Instant,
    rebon_exe_path: PathBuf,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("rebon-spawn-session-host".into())
        .spawn(
            move || match spawn_queued_worker_now(&store, &job_id, &rebon_exe_path) {
                Ok(job) => tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    %session_id,
                    %job_id,
                    worker_pid = ?job.process.pid,
                    "rebon startup: session host started"
                ),
                Err(err) => tracing::warn!(
                    error = %err,
                    %job_id,
                    "rebon startup: could not spawn the session host from here; the supervisor will"
                ),
            },
        )?;
    Ok(())
}

#[cfg(test)]
mod tests {
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

    /// The startup route writes the session before it starts the worker:
    /// a transcript file the worker can resume, a `Foreground` job named
    /// for the session, queued `resume_only` — and it takes no lock, so
    /// the worker can. The spawn itself is not exercised here (it would
    /// start a real process).
    #[test]
    fn a_hosted_startup_names_the_session_and_queues_a_foreground_worker() {
        let root = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(root.path().join("jobs"));
        let cwd = root.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_str = cwd.to_string_lossy().to_string();

        let hosted = prepare_hosted_session(&store, root.path(), runtime(), &cwd_str).unwrap();

        let state = store.read_state(&hosted.job_id).unwrap();
        assert_eq!(
            state.identity.session_id.as_deref(),
            Some(hosted.session_id.as_str())
        );
        assert_eq!(state.lease.placement, JobPlacement::Foreground);
        assert_eq!(state.process.status, BackgroundJobStatus::Queued);
        assert!(
            state.identity.resume_only,
            "nothing to run: the worker parks on the session"
        );
        assert!(state.identity.pending_prompts.is_empty());
        assert_eq!(state.identity.name, session_job_name(&hosted.session_id));
        assert!(
            !rebon_session::is_session_active(root.path(), &cwd_str, &hosted.session_id),
            "the terminal takes no lock; the worker will"
        );
        let transcript =
            rebon_session::ensure_session_file_path(root.path(), &cwd_str, &hosted.session_id)
                .unwrap();
        assert!(
            transcript.is_file(),
            "an empty transcript the worker can resume"
        );
        assert!(
            rebon_session::load_session_created_at_ms(root.path(), &cwd_str, &hosted.session_id)
                .is_some(),
            "an empty transcript dates nothing, so the session records its own age"
        );
    }

    /// A session that already exists is hosted the same way, minus the
    /// naming: one `Foreground` job, queued `resume_only` on the session's
    /// own cwd, no lock taken here.
    #[test]
    fn an_existing_session_is_given_a_foreground_worker_without_a_local_lock() {
        let root = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(root.path().join("jobs"));
        let cwd = root.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_str = cwd.to_string_lossy().to_string();
        // Never started: `spawn` is false.
        let exe = root.path().join("no-such-rebon-build").join("rebon.exe");

        let job_id = host_existing_session(
            &store,
            "sess-never-hosted",
            &cwd_str,
            runtime(),
            false,
            &exe,
        )
        .unwrap();

        let state = store.read_state(&job_id).unwrap();
        assert_eq!(
            state.identity.session_id.as_deref(),
            Some("sess-never-hosted")
        );
        assert_eq!(state.lease.placement, JobPlacement::Foreground);
        assert_eq!(state.process.status, BackgroundJobStatus::Queued);
        assert!(state.identity.resume_only);
        assert_eq!(state.identity.cwd, cwd_str);
        assert_eq!(state.identity.name, session_job_name("sess-never-hosted"));
        assert!(
            state.process.pid.is_none(),
            "not spawned: the test asked for the record only"
        );
        assert!(!rebon_session::is_session_active(
            root.path(),
            &cwd_str,
            "sess-never-hosted"
        ));
    }

    /// The job name keeps the last eight characters of the id, whole
    /// characters rather than bytes.
    #[test]
    fn a_session_job_is_named_for_the_tail_of_its_id() {
        assert_eq!(session_job_name("sess-0123456789ab"), "session 456789ab");
        assert_eq!(session_job_name("abc"), "session abc");
        assert_eq!(
            session_job_name("会话一二三四五六七八"),
            "session 一二三四五六七八"
        );
    }

    /// `--resume <session>` asks where a session lives before opening it
    /// here: the job that names it, preferring one whose worker is up, and
    /// never one that is on its way out.
    #[test]
    fn a_session_is_found_on_the_job_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
        assert!(home_job_for_session(&store, "sess-nowhere")
            .unwrap()
            .is_none());

        let mut stopped = store
            .create_job("first life".into(), PathBuf::from("."), runtime())
            .unwrap();
        stopped.process.status = BackgroundJobStatus::Stopped;
        stopped.identity.session_id = Some("sess-hosted".into());
        stopped.process.updated_at_ms = 10;
        store.write_state(&stopped).unwrap();
        assert_eq!(
            home_job_for_session(&store, "sess-hosted")
                .unwrap()
                .map(|job| job.identity.job_id),
            Some(stopped.identity.job_id.clone()),
            "a stopped job is still where the session lives"
        );

        let mut live = store
            .create_job("second life".into(), PathBuf::from("."), runtime())
            .unwrap();
        live.process.status = BackgroundJobStatus::Idle;
        live.identity.session_id = Some("sess-hosted".into());
        live.process.pid = Some(std::process::id());
        live.process.updated_at_ms = 5;
        store.write_state(&live).unwrap();
        assert_eq!(
            home_job_for_session(&store, "sess-hosted")
                .unwrap()
                .map(|job| job.identity.job_id),
            Some(live.identity.job_id.clone()),
            "a job with a live worker wins over a newer one without"
        );

        let mut leaving = store
            .create_job("leaving".into(), PathBuf::from("."), runtime())
            .unwrap();
        leaving.identity.session_id = Some("sess-leaving".into());
        leaving.process.removal_reserved = true;
        store.write_state(&leaving).unwrap();
        assert!(
            home_job_for_session(&store, "sess-leaving")
                .unwrap()
                .is_none(),
            "a job reserved for removal is not a home"
        );

        let mut respawned = store
            .create_job("respawned".into(), PathBuf::from("."), runtime())
            .unwrap();
        respawned.identity.session_id = Some("sess-respawned".into());
        respawned.identity.respawned_job_id = Some("bg-successor".into());
        store.write_state(&respawned).unwrap();
        assert!(
            home_job_for_session(&store, "sess-respawned")
                .unwrap()
                .is_none(),
            "a job already respawned is not a home"
        );
    }

    /// The two pickers answer different questions, and the difference is
    /// what a caller must choose between: the latest record of a session
    /// includes one on its way out, its home does not.
    #[test]
    fn the_latest_record_of_a_session_is_not_always_its_home() {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());

        let mut home = store
            .create_job("home".into(), PathBuf::from("."), runtime())
            .unwrap();
        home.identity.session_id = Some("sess-both".into());
        home.process.status = BackgroundJobStatus::Idle;
        home.process.updated_at_ms = 10;
        store.write_state(&home).unwrap();

        let mut leaving = store
            .create_job("leaving".into(), PathBuf::from("."), runtime())
            .unwrap();
        leaving.identity.session_id = Some("sess-both".into());
        leaving.process.removal_reserved = true;
        leaving.process.updated_at_ms = 20;
        store.write_state(&leaving).unwrap();

        assert_eq!(
            crate::latest_job_for_session(&store, "sess-both")
                .unwrap()
                .map(|job| job.identity.job_id),
            Some(leaving.identity.job_id.clone())
        );
        assert_eq!(
            home_job_for_session(&store, "sess-both")
                .unwrap()
                .map(|job| job.identity.job_id),
            Some(home.identity.job_id.clone())
        );
    }
}
