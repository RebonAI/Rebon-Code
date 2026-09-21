//! The default way a session starts: name it, start its worker, become its
//! mirror.
//!
//! RFC-0004 §9. The terminal used to build a whole engine, take the
//! session's lock, and only then draw — and a session that was to live in a
//! worker was handed over *after* all that, so the worker's start followed
//! the local setup instead of overlapping it. Here the order is turned
//! around: before anything local is built, the session gets an id, a
//! transcript file, a job record, and a spawned worker. The local build then
//! runs while the worker boots, and the event loop mirrors the worker once
//! it publishes an endpoint — nothing here waits for it.
//!
//! The same route serves `/new` in a hosted terminal and a `--resume` of a
//! session no job names (U5b): the session is given a worker first and the
//! terminal mirrors it from birth, rather than being built here and handed
//! over afterwards.
//!
//! The hosting itself is `rebon-session-host`'s
//! ([`rebon_session_host::start_hosted_session`],
//! [`rebon_session_host::host_existing_session`]), shared with `serve`. What
//! stays here is the terminal's part: resolving the runtime, store, projects
//! root and executable it hosts with, and deciding whether to host at all.

use crate::background::RuntimeFieldsExt;
use crate::rebon_config::RuntimeOverride;
use rebon_session_host::HostedStartup;

/// Name a brand-new session and start the worker that will host it.
///
/// See [`rebon_session_host::start_hosted_session`]. Fails only when nothing
/// was started — the caller then hosts the session in-process, as `--local`
/// would. A spawn that fails leaves the job queued for the supervisor; the
/// terminal meanwhile shows the dot, and after 30 s the notice.
pub(crate) fn start_hosted_session(
    overrides: &RuntimeOverride,
    cwd: &str,
) -> anyhow::Result<HostedStartup> {
    // The worker starts under the mode this terminal is about to show, so
    // the two agree from the first frame: on attach, the mode the worker
    // publishes is the one the UI adopts.
    let mut resolved = overrides.clone();
    resolved.permission_mode = resolved
        .permission_mode
        .or_else(crate::rebon_config::saved_default_permission_mode);
    let runtime = crate::background::BackgroundRuntimeFields::from_runtime_override(&resolved);
    start_hosted_session_with_runtime(runtime, cwd)
}

/// [`start_hosted_session`] for a runtime already resolved — what `/new`
/// in a hosted terminal has, from the session it is leaving.
pub(crate) fn start_hosted_session_with_runtime(
    runtime: crate::background::BackgroundRuntimeFields,
    cwd: &str,
) -> anyhow::Result<HostedStartup> {
    rebon_session_host::start_hosted_session(
        &crate::background::cli_default_store(),
        &rebon_harness::projects_root(),
        runtime,
        cwd,
        &crate::background::rebon_exe(),
    )
}

/// What the session-host decision leaves for the run below.
pub(crate) struct SessionHostDecision {
    /// Whether the first frame can go out before the session is built.
    /// Only a brand-new session qualifies: a resume, an
    /// attach and the agent list all have to decide *which* session this is
    /// before anything is drawn for it, and first-run onboarding needs the
    /// session's hooks. `--local` qualifies — its build, too, is behind the
    /// frame.
    pub(crate) draw_before_session: bool,
    pub(crate) hosted: Option<HostedStartup>,
}

/// Hosted by default: a brand-new session gets its worker
/// started here, before the local build, so the two overlap; the local
/// session is then built as that worker's mirror. Everything that first
/// has to decide *which* session this is — a resume, an attach, the
/// agent list — keeps a placeholder session and hands it over once the
/// choice is made (`start_hosted_session_if_requested`). `--local` keeps
/// the whole thing in this process.
pub(crate) fn decide_session_host(
    overrides: &mut RuntimeOverride,
    cwd: &std::path::Path,
    has_startup_resume: bool,
    startup_started: std::time::Instant,
) -> SessionHostDecision {
    let mut host_failed = false;
    let draw_before_session = !has_startup_resume
        && overrides.resume.is_none()
        && overrides.attached_background_job_id.is_none()
        && !overrides.startup_agent_view
        && !crate::tui::onboarding_dialog::first_run_wizard_due();
    let hosted_startup = if overrides.startup_local
        || has_startup_resume
        || overrides.resume.is_some()
        || overrides.attached_background_job_id.is_some()
        || overrides.startup_agent_view
    {
        None
    } else {
        match start_hosted_session(overrides, &cwd.to_string_lossy()) {
            Ok(hosted) => Some(hosted),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "rebon startup: could not start a session host; hosting in this process"
                );
                host_failed = true;
                overrides.startup_notices.push(format!(
                    "Could not start a background worker for this session ({err:#}). It runs in this process instead, as with --local: closing the terminal ends it."
                ));
                None
            }
        }
    };
    if let Some(hosted) = &hosted_startup {
        overrides.resume = Some(hosted.session_id.clone());
        overrides.attached_background_job_id = Some(hosted.job_id.clone());
        overrides.startup_hosted = false;
    } else if !overrides.startup_local
        && !host_failed
        && overrides.attached_background_job_id.is_none()
    {
        overrides.startup_hosted = true;
    }
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        hosted = hosted_startup.is_some(),
        local = overrides.startup_local,
        "rebon startup: session host decided"
    );
    SessionHostDecision {
        draw_before_session,
        hosted: hosted_startup,
    }
}
