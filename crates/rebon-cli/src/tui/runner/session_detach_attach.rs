//! Detach/attach plumbing between the live TUI session and the
//! background-agent ("agent view") layer. Owns the helpers that hand
//! the current session to a background worker (`hand_over`,
//! `background_current_session`, `detach_current_session_to_agent_view`,
//! `stop_attached_background_session*`), dispatch new prompts from agent
//! view (`dispatch_agent_view_prompt`, `dispatch_agent_view_prompt_and_attach`),
//! build the prompt a dispatch carries (`prompt_from_paste_contents`), and
//! route every `AgentViewKeyOutcome` (`handle_agent_view_outcome`)
//! including the recap message rendered when an attached job rejoins the
//! foreground (`background_attach_recap_message`).
//!
//! A turn never moves between processes: a
//! session is handed over between turns, and a turn in flight is finished
//! or cancelled first. The in-process detach that used to carry a running
//! turn into a job record is gone with the hosted default — a hosted
//! session's turn runs in its worker already, and a `--local` one is
//! refused a handover until the turn is over.

use std::path::{Path, PathBuf};

use tokio::runtime::Handle;

use crate::background::{probe_hosted_wait, settle_hosted_wait, HostedWaitProbe, HostedWaitReport};
// The cadence and the budgets are only asserted on here now: the poll reads
// them on the session side.
#[cfg(test)]
use crate::background::{
    hosted_probe_interval, HOSTED_PROBE_EAGER_FOR, HOSTED_PROBE_INTERVAL,
    HOSTED_PROBE_INTERVAL_EAGER, HOSTED_STARTUP_SLOW_NOTICE, HOSTED_WAIT_GIVE_UP_AFTER,
};
use crate::session::submit_payload::SubmitPayload;
use crate::tui::agent_view::AgentViewKeyOutcome;
use crate::tui::app::AppState;
use crate::tui::dispatch::commit_submit_payload_to_transcript;
use crate::tui::permission_modal::PendingPermission;
use crate::tui::wiring::TuiEngineSession;

use super::agent_view::{
    agent_view_dispatch_cwd, background_prompt_for_detach, has_active_detach_blocking_tasks,
    has_terminal_bound_tasks, move_agent_view_job, open_agent_view, open_agent_view_with_store,
    persist_agent_view_grouping_preference, refresh_agent_view, session_already_has_background_job,
};
use super::prompt_lifecycle::prepare_for_new_prompt_after_withdrawal;
use super::remote_background_attachment::{park_attachment, parked_session_hint};
use super::resume_selection::park_session_on_job;
use super::ActivePrompt;
use crate::session_shell::handover::{
    background_runtime_from_session, dispatching_worker_job_id, release_then_stop_worker,
    retake_lock_after_failed_handover, stop_agent_view_job_in_store, StoppedSession, StoppedWorker,
};
use rebon_session_host::session_job_name;

pub(super) fn detach_current_session_to_agent_view(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    active_prompt: &mut Option<ActivePrompt>,
) {
    if session_already_has_background_job(
        &session.session_id,
        session.attached_background_job_id.as_deref(),
    ) {
        open_agent_view(app, session.attached_background_job_id.as_deref());
        return;
    }
    if app.rebon_tui.transcript.is_empty() && active_prompt.is_none() {
        open_agent_view(app, session.attached_background_job_id.as_deref());
        return;
    }
    let active_tasks = has_active_detach_blocking_tasks(app);
    if active_tasks > 0 {
        super::inject_system_message(
            app,
            "error",
            &format!(
                "Cannot open agents — {active_tasks} background task(s) running. Use /bg to background explicitly."
            ),
        );
        app.follow_transcript_tail = true;
        return;
    }
    background_current_session(app, session, None, active_prompt);
}

pub(super) fn background_current_session(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    final_prompt: Option<String>,
    active_prompt: &mut Option<ActivePrompt>,
) {
    if let Some(remote) = session.remote_background_attachment.take() {
        if let Some(prompt) = final_prompt {
            if let Err(err) = crate::background::reply_to_background_job(&remote.job_id, prompt) {
                session.remote_background_attachment = Some(remote);
                super::inject_system_message(
                    app,
                    "error",
                    &format!("Failed to send final background prompt: {err}"),
                );
                app.follow_transcript_tail = true;
                return;
            }
        }
        // `/bg` means the same thing whichever side of the mirror it is
        // typed on: this job is nobody's now, keep it running. The local
        // branch below has always cut the parent link here; a mirrored
        // child skipping it left the job attached to the worker that
        // started it, so the next parent stop took it down anyway — the
        // exact opt-out the user just asked for.
        let store = crate::background::cli_default_store();
        // The detach is the whole point of the command, so a failed write is
        // a failed `/bg`: the job is still the parent's, and reporting
        // "detached" would leave the user believing work is safe that the
        // next parent stop will take down. Say what happened and keep the
        // session where it is, so the command can be retried.
        match crate::background::detach_background_job_from_parent(&store, &remote.job_id) {
            Ok(cut_loose) => {
                session.attached_background_job_id = None;
                app.is_loading = false;
                super::apply_new_session(app, session);
                app.plan_entries.clear();
                open_agent_view(app, None);
                if let Some(view) = app.agent_view.as_mut() {
                    let mut status = format!("detached from {}", remote.job_id);
                    if cut_loose {
                        status.push_str("; it now outlives the worker that started it");
                    }
                    view.status = Some(status);
                }
            }
            Err(err) => {
                let job_id = remote.job_id.clone();
                session.remote_background_attachment = Some(remote);
                super::inject_system_message(
                    app,
                    "error",
                    &format!(
                        "Could not cut {job_id} loose from the worker that started it: {err}. It is still that worker's, and would be stopped with it — the session was left attached so you can retry."
                    ),
                );
                app.follow_transcript_tail = true;
            }
        }
        return;
    }

    // A turn running in this process owns the engine and is writing the
    // transcript; it cannot be moved to another process, and a job record
    // that pretends otherwise gives the session two owners.
    // The turn ends first — on its own, or by Ctrl+C.
    if active_prompt.is_some() {
        super::inject_system_message(app, "error", MID_TURN_HANDOVER_REFUSAL);
        app.follow_transcript_tail = true;
        return;
    }
    let store = crate::background::cli_default_store();
    let should_queue = final_prompt.is_some();
    let prompt_update = final_prompt.clone();
    let name = final_prompt.clone().or_else(|| {
        session
            .attached_background_job_id
            .is_none()
            .then(|| session_job_name(&session.session_id))
    });
    let runtime = background_runtime_from_session(
        session,
        session.ui_mode,
        app.effort_level,
        app.permission_mode,
    );
    if let Some(job_id) = session.attached_background_job_id.clone() {
        match crate::background::mark_existing_background_session_idle(
            &store,
            &job_id,
            prompt_update.clone(),
            PathBuf::from(&session.cwd),
            runtime.clone(),
            session.session_id.clone(),
            name.clone(),
        ) {
            Ok(job) => {
                if should_queue {
                    if let Err(err) = crate::background::queue_background_job(&job.job_id()) {
                        super::inject_system_message(
                            app,
                            "error",
                            &format!("Failed to queue background job {}: {err}", job.job_id()),
                        );
                        app.follow_transcript_tail = true;
                        return;
                    }
                }
                // Backgrounding this job *is* the opt-out from its parent's
                // lifetime: the user came into the child on purpose and said
                // "keep running". From here it outlives the worker that
                // started it — and if that write fails the job is still the
                // parent's, so the command says so rather than reporting a
                // detachment that did not happen.
                let detached =
                    crate::background::detach_background_job_from_parent(&store, &job.job_id());
                // `apply_new_session` wipes the transcript, so a message
                // injected before it would be gone by the time the user
                // could read it. The failure is carried across and told
                // afterwards — and in the Agent View status, which is where
                // the eye lands next.
                super::apply_new_session(app, session);
                open_agent_view(app, None);
                if let Some(view) = app.agent_view.as_mut() {
                    let mut status = if should_queue {
                        format!(
                            "backgrounded session as {} and queued final prompt",
                            job.job_id()
                        )
                    } else {
                        format!("backgrounded session as {}", job.job_id())
                    };
                    match &detached {
                        Ok(true) => status.push_str("; it now outlives the worker that started it"),
                        Ok(false) => {}
                        Err(_) => status.push_str(
                            "; it could NOT be cut loose from the worker that started it",
                        ),
                    }
                    view.status = Some(status);
                }
                if let Err(err) = detached {
                    super::inject_system_message(
                        app,
                        "error",
                        &format!(
                            "Backgrounded this session as {}, but it could not be cut loose from the worker that started it: {err}. It would be stopped with that worker — retry /bg from Agent View.",
                            job.job_id()
                        ),
                    );
                    app.follow_transcript_tail = true;
                }
                return;
            }
            Err(err) => {
                super::inject_system_message(
                    app,
                    "error",
                    &format!("Failed to background session: {err}"),
                );
                app.follow_transcript_tail = true;
                return;
            }
        }
    }
    // An idle local session: the same handover `/hosted` makes, with the
    // terminal leaving afterwards instead of staying to watch.
    hand_over(
        app,
        session,
        &*active_prompt,
        HandOverThen::Leave { final_prompt },
    );
}

/// What `/bg`, Ctrl+Z and `/exit` say to a turn in flight in this process.
/// Invariant I6: no handover mid-turn.
pub(super) const MID_TURN_HANDOVER_REFUSAL: &str = "Finish or cancel the running turn first (Ctrl+C cancels it) — a turn in flight cannot be moved to a background worker.";

/// Move this session into a background worker and keep mirroring it.
///
/// `/exit` already hands a session to a worker and walks away; this is the
/// same handover with the TUI staying on it. Afterwards the engine that owns
/// this conversation lives in a process that outlives the terminal, so
/// closing the TUI detaches instead of killing — and every sub-agent the
/// session spawns lives in that worker too, which is the whole point (see
/// an out-of-tree survey).
pub(super) fn host_current_session_in_worker(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    active_prompt: &Option<ActivePrompt>,
) {
    hand_over(app, session, active_prompt, HandOverThen::Stay);
}

/// What this terminal does once it has handed a session to a worker.
///
/// Every gesture that makes a session "background" — `/hosted`, two Lefts,
/// `/bg`, Ctrl+Z — hands it to a worker the same way. They differ only
/// here: whether the terminal stays on the session as its mirror, or leaves
/// it for a fresh session and the Agent View.
pub(super) enum HandOverThen {
    /// Stay on the session and mirror it (`/hosted`, two Lefts).
    Stay,
    /// Leave it: open a fresh session and the Agent View (`/bg`, Ctrl+Z). A
    /// final prompt is queued on the worker before leaving.
    Leave { final_prompt: Option<String> },
}

/// Hand the session on screen to a background worker.
///
/// One path for every gesture, so they agree on what can be handed over: a
/// session without a turn in flight, that exists on disk, and that no other
/// process has claimed. What happens afterwards is `then`'s.
pub(super) fn hand_over(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    active_prompt: &Option<ActivePrompt>,
    then: HandOverThen,
) {
    if let Some(remote) = session.remote_background_attachment.as_ref() {
        let message = if remote.is_live() {
            "This session already runs in a background worker.".to_string()
        } else {
            format!(
                "This session belongs to worker {}, which is stopped — type a prompt to continue it in a new worker.",
                remote.job_id
            )
        };
        super::inject_local_command_feedback(app, "hosted", &message);
        app.follow_transcript_tail = true;
        return;
    }
    if let Some(pending) = session.pending_hosted_session.as_ref() {
        super::inject_system_message(
            app,
            "notice",
            &format!(
                "Already handing this session to {} — waiting for it.",
                pending.job_id
            ),
        );
        app.follow_transcript_tail = true;
        return;
    }
    // A running local turn owns the engine and is still writing the
    // transcript. Handing over mid-turn would give the worker a session
    // another process has open — the two-owner state the attach path exists
    // to refuse.
    if active_prompt.is_some() {
        super::inject_system_message(app, "error", MID_TURN_HANDOVER_REFUSAL);
        app.follow_transcript_tail = true;
        return;
    }
    // Local agents and tasks are this process's, not the session's: they
    // hold no worker of their own, so the handover cannot take them along
    // and closing this terminal afterwards stops them. Promising a session
    // that "keeps running when you close the terminal" while quietly
    // leaving work behind to be killed is the one outcome `/hosted` must
    // not produce, so say what is in the way and let the user decide.
    //
    // Backgrounded tasks count here: that flag moves a task to the
    // background indicator, it does not move the task to another process.
    //
    // Leaving is different: the terminal stays alive on a fresh session, and
    // the tasks keep running in it exactly as before.
    let active_tasks = has_terminal_bound_tasks(app);
    if matches!(then, HandOverThen::Stay) && active_tasks > 0 {
        super::inject_system_message(
            app,
            "error",
            &format!(
                "Cannot move this session into a worker — {active_tasks} task(s) are running in this terminal and would not come along (backgrounded ones included: /bg changes where they are shown, not where they run). Wait for them, or stop them first."
            ),
        );
        app.follow_transcript_tail = true;
        return;
    }

    if let Err(err) = session.ensure_transcript_on_disk() {
        super::inject_system_message(
            app,
            "error",
            &format!("Could not prepare this session for a background worker: {err}"),
        );
        app.follow_transcript_tail = true;
        return;
    }

    let store = crate::background::cli_default_store();
    let runtime = background_runtime_from_session(
        session,
        session.ui_mode,
        app.effort_level,
        app.permission_mode,
    );
    let final_prompt = match &then {
        HandOverThen::Stay => None,
        HandOverThen::Leave { final_prompt } => final_prompt.clone(),
    };
    // A job is named after its final prompt when there is one, after the
    // session otherwise — unless it already has a name from an earlier life.
    let name = final_prompt.clone().or_else(|| {
        session
            .attached_background_job_id
            .is_none()
            .then(|| session_job_name(&session.session_id))
    });
    // Without a final prompt the handover must not start a turn: the job is
    // adopted idle and then warmed `resume_only`, so the worker resumes this
    // session and parks, waiting for whatever comes next.
    let prompt = background_prompt_for_detach(&session.session_id, final_prompt.as_deref());
    // Staying means somebody is watching, and when they stop the host should
    // let go sooner than a background job would. Leaving says the opposite —
    // the terminal moves on — so the worker keeps a background job's hour.
    let placement = match then {
        HandOverThen::Stay => rebon_session_host::JobPlacement::Foreground,
        HandOverThen::Leave { .. } => rebon_session_host::JobPlacement::Background,
    };
    let job = match crate::background::adopt_existing_background_session(
        &store,
        prompt,
        PathBuf::from(&session.cwd),
        runtime,
        session.session_id.clone(),
        name,
        false,
        placement,
    ) {
        Ok(job) => job,
        Err(err) => {
            super::inject_system_message(
                app,
                "error",
                &format!("Could not hand this session to a background worker: {err}"),
            );
            app.follow_transcript_tail = true;
            return;
        }
    };

    if let HandOverThen::Leave { final_prompt } = then {
        // The worker picks the job up from the queue; nothing here waits for
        // it. This terminal is done with the session, and a fresh one takes
        // its place — which is also what releases the session's lock.
        super::apply_new_session(app, session);
        if final_prompt.is_some() {
            if let Err(err) = crate::background::queue_background_job(&job.job_id()) {
                super::inject_system_message(
                    app,
                    "error",
                    &format!("Failed to queue background job {}: {err}", job.job_id()),
                );
                app.follow_transcript_tail = true;
            }
        }
        open_agent_view(app, None);
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some(if final_prompt.is_some() {
                format!(
                    "backgrounded session as {} and queued final prompt",
                    job.job_id()
                )
            } else {
                format!("backgrounded session as {}", job.job_id())
            });
        }
        return;
    }
    // The worker resumes this very session, and resuming takes the session's
    // active lock — which this process is holding. Release it before the
    // worker starts or it cannot resume at all ("session is still active"),
    // and the handover would sit waiting for a worker that can never come
    // up. `/exit` gets this for free by swapping the session out; staying on
    // the session means giving the claim up explicitly.
    drop(session.session_active_lock.take());

    // Queued, then spawned from here rather than left to the supervisor's
    // tick: somebody is watching this happen. A spawn that fails leaves the
    // job queued for the supervisor the warm-up made sure of.
    let warmed = crate::background::warm_background_job_for_peek(&job.job_id());
    let warm_failure = match warmed {
        Ok(true) => rebon_session_host::spawn_queued_worker_now(
            &store,
            &job.job_id(),
            &crate::background::rebon_exe(),
        )
        .err()
        .map(|err| format!("Could not start the worker for {}: {err}", job.job_id())),
        Ok(false) => Some(format!(
            "Background job {} could not be warmed for handover (it may already be busy).",
            job.job_id()
        )),
        Err(err) => Some(format!(
            "Could not start the worker for {}: {err}",
            job.job_id()
        )),
    };
    if let Some(mut message) = warm_failure {
        // Nothing was started, so this session can keep working here — but
        // only if the claim comes back. A claim we cannot retake belongs to
        // someone who can still write this session, so the session is
        // parked on the job rather than left quietly submittable: the
        // next prompt gives the job a worker, as it would after `/stop`.
        if !retake_lock_after_failed_handover(session, &job.job_id()) {
            session.attached_background_job_id = Some(job.job_id().to_string());
            park_session_on_job(app, session, &job.job_id(), job.status());
            message.push_str(&format!(
                " This session also could not reclaim its active lock, so it stays with {}. {}",
                job.job_id(),
                parked_session_hint()
            ));
        }
        super::inject_system_message(app, "error", &message);
        app.follow_transcript_tail = true;
        return;
    }

    session.attached_background_job_id = Some(job.job_id().to_string());
    session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
        job.job_id().to_string(),
    ));
    // Command feedback, not a system notice: a generic info-level system
    // row renders as nothing, and the user who typed `/hosted` is waiting
    // to be told it took.
    super::inject_local_command_feedback(
        app,
        "hosted",
        &format!(
            "Moving this session into background worker {} — it will keep running when you close the terminal.",
            job.job_id()
        ),
    );
    app.follow_transcript_tail = true;
}

/// How long two presses may be apart and still count as one gesture. Same
/// window `Esc Esc` uses — one muscle-memory speed, not two.
const DOUBLE_PRESS_WINDOW_MS: u64 = 400;

/// Two Lefts on an empty prompt hand this session to a background worker.
///
/// `←` on an empty input has nothing to move through, so the gesture costs
/// nothing that existed before. It means what `/hosted` means: from here on
/// the engine that owns this conversation — and every agent it spawns — runs
/// in a process that outlives the terminal. Not `Esc Esc` (rewind owns it),
/// and not a printable prefix, which would collide with the first character
/// of a message.
///
/// Returns whether the gesture fired; the caller falls through to ordinary
/// cursor movement when it did not.
pub(super) fn host_session_on_double_left(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    active_prompt: &Option<ActivePrompt>,
) -> bool {
    // A non-empty prompt means the key is doing its day job, and any surface
    // that is not the prompt has its own meaning for Left.
    if !app.input.is_empty() || app.agent_view.is_some() {
        app.last_left_press_ms = 0;
        return false;
    }
    let now_ms = super::status_bar::wall_clock_ms();
    let is_double_press = app.last_left_press_ms != 0
        && now_ms.saturating_sub(app.last_left_press_ms) <= DOUBLE_PRESS_WINDOW_MS;
    app.last_left_press_ms = if is_double_press { 0 } else { now_ms };
    if !is_double_press {
        return false;
    }
    host_current_session_in_worker(app, session, active_prompt);
    true
}

/// Host the session once the UI has settled, and report whether it fired.
///
/// Deliberately not done at startup: with `--resume` / `--continue` a dialog
/// is still deciding *which* session this is, and the placeholder on screen
/// before that answer is not the conversation the user asked to host.
/// Waiting for the dialog to close costs a few event-loop passes and hands
/// over the right session. One-shot: the flag is cleared either way.
///
/// A session with something in it — one that was local and is now to be
/// hosted — is handed over, and says so. A blank one (the placeholder a
/// cancelled picker or a closed agent list leaves) never lived here: it is
/// replaced by a session started in a worker and mirrored from birth, the
/// way `rebon` starts, silently.
pub(super) fn start_hosted_session_if_requested(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    active_prompt: &Option<ActivePrompt>,
) -> bool {
    if !session.terminal_startup.hosted {
        return false;
    }
    if app.resume_dialog.is_some()
        || app.onboarding_dialog.is_some()
        || app.agent_view.is_some()
        || active_prompt.is_some()
    {
        return false;
    }
    // Already where `--hosted` wanted to get: `rebon attach <job> --hosted`
    // is a no-op, not an error.
    if session.remote_background_attachment.is_some() || session.pending_hosted_session.is_some() {
        session.terminal_startup.hosted = false;
        return false;
    }
    session.terminal_startup.hosted = false;
    if super::submit::current_session_is_blank(app, session)
        && super::commands::apply_new_hosted_session(app, session, false)
    {
        return true;
    }
    host_current_session_in_worker(app, session, active_prompt);
    true
}

/// Attach a session to its worker once that worker publishes its endpoint.
///
/// Polled from the event loop: the worker is starting a process and booting
/// an engine, and blocking the UI on that would freeze the very session the
/// user is watching.
pub(super) fn poll_pending_hosted_session(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
) {
    let store = crate::background::cli_default_store();
    poll_pending_hosted_session_in_store(app, session, pending_permission, &store);
}

pub(super) fn poll_pending_hosted_session_in_store(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    store: &crate::background::BackgroundStore,
) {
    let probe = probe_hosted_wait(session, store);
    let attach_started = std::time::Instant::now();
    // A handover mirrors the session already on screen, so the view stays; a
    // dispatch mirrors a job that was never on screen, so the view is rebuilt
    // from that job's transcript. A reattach is whichever of the two it says it
    // is. Only a terminal can do any of that, which is why the wait is polled
    // in two halves with this in between.
    let attached = match (
        &probe,
        session.pending_hosted_session.as_ref().map(|p| p.kind),
    ) {
        (HostedWaitProbe::Mirrorable { target, .. }, Some(kind)) => match kind {
            crate::background::PendingHostedKind::Handover
            | crate::background::PendingHostedKind::Startup
            | crate::background::PendingHostedKind::Reattach { keep_view: true } => {
                super::apply_handover_attach_target(app, session, target)
            }
            crate::background::PendingHostedKind::Dispatch
            | crate::background::PendingHostedKind::Reattach { keep_view: false } => {
                super::apply_background_attach_target(app, session, target)
            }
        },
        _ => false,
    };
    let settled = settle_hosted_wait(session, store, probe, attached);

    if let Some(job_id) = settled.slow_notice {
        super::inject_local_command_feedback(
            app,
            "hosted",
            &format!(
                "Still starting the session host ({job_id})… Anything you type is queued for it. `rebon --local` runs a session in this process instead."
            ),
        );
        app.follow_transcript_tail = true;
    }

    match settled.report {
        HostedWaitReport::Quiet => {}
        HostedWaitReport::Attached {
            job_id,
            kind,
            elapsed_ms,
            probe_ms,
            startup,
        } => {
            *pending_permission = None;
            // The ordinary start of a session says nothing: the dot in
            // the status bar goes away, and that is the whole event.
            if startup {
                tracing::info!(
                    %job_id,
                    elapsed_ms,
                    probe_ms,
                    attach_ms = attach_started.elapsed().as_millis() as u64,
                    "rebon startup: session host attached"
                );
                app.follow_transcript_tail = true;
                return;
            }
            let (label, message) = match kind {
                crate::background::PendingHostedKind::Reattach { .. } => {
                    ("attach", format!("Attached to worker {job_id}."))
                }
                _ => (
                    "hosted",
                    format!(
                        "This session now runs in worker {job_id}. Closing the terminal detaches instead of stopping it."
                    ),
                ),
            };
            super::inject_local_command_feedback(app, label, &message);
            app.follow_transcript_tail = true;
        }
        HostedWaitReport::Ended {
            job_id,
            kind,
            status,
            verb,
            detail,
            park,
        } => {
            let detail = detail
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default();
            if let Some(status) = park {
                park_waiting_session(app, session, pending_permission, &job_id, status);
            }
            let (level, message) = match kind {
                crate::background::PendingHostedKind::Dispatch => (
                    if matches!(status, crate::background::BackgroundJobStatus::Succeeded) {
                        "notice"
                    } else {
                        "error"
                    },
                    format!(
                        "Job {job_id} {verb}{detail} before this session could mirror it. Open it from Agent View, or `rebon attach {job_id}`."
                    ),
                ),
                crate::background::PendingHostedKind::Reattach { keep_view: false } => (
                    "error",
                    format!(
                        "Worker for {job_id} {verb}{detail} before it could be mirrored. Open it from Agent View again, or `rebon attach {job_id}`."
                    ),
                ),
                crate::background::PendingHostedKind::Handover
                | crate::background::PendingHostedKind::Reattach { keep_view: true } => (
                    "error",
                    format!(
                        "Worker {job_id} {verb}{detail} before this session finished moving into it. {}",
                        parked_session_hint()
                    ),
                ),
                // The session's first worker never got as far as hosting it.
                // Same parking, and the two ways out named: another worker, or
                // this process.
                crate::background::PendingHostedKind::Startup => (
                    "error",
                    format!(
                        "The worker for this session ({job_id}) {verb}{detail} before it came up. {} `rebon --local` runs a session in this process instead.",
                        parked_session_hint()
                    ),
                ),
            };
            // An info-level system row renders as nothing; the good news goes
            // out as command feedback so it is actually read.
            if level == "notice" {
                super::inject_local_command_feedback(app, "agents", &message);
            } else {
                super::inject_system_message(app, level, &message);
            }
            app.follow_transcript_tail = true;
        }
        HostedWaitReport::GaveUp {
            job_id,
            kind,
            budget_secs: budget,
            reason,
            park,
        } => {
            let detail = reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();
            if let Some(status) = park {
                park_waiting_session(app, session, pending_permission, &job_id, status);
            }
            let message = match kind {
                // Nothing was handed over — the job is running on its own and
                // only the view is missing, so point at the two ways to get it
                // back rather than making this sound like lost work.
                crate::background::PendingHostedKind::Dispatch
                | crate::background::PendingHostedKind::Reattach { keep_view: false } => format!(
                    "Worker {job_id} did not come up within {budget}s{detail}. The job keeps running on its own — watch it in Agent View, or attach with `rebon attach {job_id}`."
                ),
                // Giving up waiting is not the same as getting the session
                // back. The job is not over — it is slow — so a worker may
                // still come up and resume this very session.
                crate::background::PendingHostedKind::Handover
                | crate::background::PendingHostedKind::Reattach { keep_view: true } => format!(
                    "Worker {job_id} did not come up within {budget}s{detail}. {}",
                    parked_session_hint()
                ),
                crate::background::PendingHostedKind::Startup => format!(
                    "The worker for this session ({job_id}) did not come up within {budget}s{detail}. {} `rebon --local` runs a session in this process instead.",
                    parked_session_hint()
                ),
            };
            super::inject_system_message(app, "error", &message);
            app.follow_transcript_tail = true;
        }
    }
}

/// A session that was waiting for `job_id`'s worker stops waiting and
/// stays the job's.
///
/// A mirror whose worker died already has an attachment (kept through the
/// wait); it lets the worker go. A handover has none yet — the session is
/// on screen as a local one whose lock was given away — so one is made for
/// it, without a worker, over the rows already there.
fn park_waiting_session(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    job_id: &str,
    status: crate::background::BackgroundJobStatus,
) {
    if session.remote_background_attachment.is_some() {
        park_attachment(app, session, pending_permission, status);
    } else if !park_session_on_job(app, session, job_id, status) {
        session.attached_background_job_id = None;
    }
}

/// What `/stop` did, and the one line to say it with.
pub(super) struct StoppedAttachedSession {
    pub feedback: String,
}

pub(super) fn stop_attached_background_session(
    app: &mut AppState,
    session: &mut TuiEngineSession,
) -> anyhow::Result<StoppedAttachedSession> {
    let store = crate::background::cli_default_store();
    stop_attached_background_session_in_store(app, session, &store)
}

/// `/stop`: stop the worker, keep the session.
///
/// The worker is the only thing stopped. The session on screen stays the
/// job's, parked: every row it had, and the next prompt typed into it gives
/// the job a worker again — the same explicit act as `rebon attach`. It does
/// not come back to this process, and the list is a Ctrl+Z away.
///
/// Letting the worker go and stopping it is one call into the session, in that
/// order and for the reason [`StoppedSession`] gives. What is left here is the
/// screen: the overlay and the counters this session was showing for a worker
/// it no longer has, the parking that needs somewhere to park, and the sentence.
pub(super) fn stop_attached_background_session_in_store(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    store: &crate::background::BackgroundStore,
) -> anyhow::Result<StoppedAttachedSession> {
    let StoppedWorker {
        job_id,
        tree,
        session_is,
    } = release_then_stop_worker(session, store)?;
    app.rebon_tui.overlay.clear();
    app.plan_entries.clear();
    app.streaming_token_count = 0;
    app.usage_mut().clear_last_turn();
    app.background_agent_tool_tasks.clear();
    app.remote_background_tasks.clear();
    app.is_loading = false;

    let parked = match session_is {
        StoppedSession::Parked => true,
        StoppedSession::ParkOnJob => park_session_on_job(
            app,
            session,
            &job_id,
            crate::background::BackgroundJobStatus::Stopped,
        ),
        StoppedSession::NotTheJobs => false,
    };
    if !parked {
        // A dispatched job this session was only waiting to mirror, or an
        // attach-here turn: the session was never the job's, so it stays a
        // local one and the list says what was stopped.
        session.attached_background_job_id = None;
        session.remote_background_attachment = None;
        open_agent_view_with_store(app, store, Some(stopped_tree_status(&job_id, &tree)), None);
        return Ok(StoppedAttachedSession {
            feedback: format!("Stopped {job_id}."),
        });
    }
    let mut feedback = format!("Worker {job_id} stopped");
    if !tree.stopped_children.is_empty() {
        feedback.push_str(&format!(
            " with {} it started",
            match tree.stopped_children.len() {
                1 => "the agent".to_string(),
                count => format!("the {count} agents"),
            }
        ));
    }
    feedback.push('.');
    for (child, error) in &tree.failed_children {
        feedback.push_str(&format!(" Could not stop {child}: {error}."));
    }
    feedback.push(' ');
    feedback.push_str(parked_session_hint());
    Ok(StoppedAttachedSession { feedback })
}

fn prompt_from_paste_contents(
    text: String,
    images: Vec<rebon_types::PromptPasteContent>,
) -> (String, Vec<crate::background::BackgroundImageAttachment>) {
    let expanded = crate::tui::dispatch::expand_paste_references(&text, &images);
    let stored_images = images
        .into_iter()
        .filter(|image| image.kind == "image")
        .map(crate::background::BackgroundImageAttachment::from_prompt_paste_content)
        .collect();
    (expanded, stored_images)
}

fn dispatch_agent_view_prompt_and_attach(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    prompt: String,
    images: Vec<rebon_types::PromptPasteContent>,
) {
    let prompt = prompt.trim().to_string();
    let (prompt, images) = prompt_from_paste_contents(prompt, images);
    if active_prompt.is_some() || app.is_loading {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some("cannot dispatch and attach while a prompt is running".to_string());
        }
        return;
    }
    let dispatch = crate::background::resolve_background_dispatch_prompt_with_skills(
        &prompt,
        Path::new(&session.cwd),
        &session.engine_half.skill_registry.ids(),
    );
    if dispatch.prompt.trim().is_empty() && images.is_empty() {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some("background prompt is empty".to_string());
        }
        return;
    }
    // `@repo` names a directory, and resolving it already stripped the token
    // from the prompt. Enter and Ctrl+N can honour it because they dispatch
    // into a worker that gets its own cwd; running here cannot — this
    // session's directory is fixed — so the alternative is to work in the
    // wrong repository with the request quietly deleted.
    if let Some(target) = dispatch
        .cwd
        .as_ref()
        .map(|cwd| cwd.to_string_lossy().to_string())
        .filter(|target| !rebon_session::same_cwd(target, &session.cwd))
    {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some(format!(
                "{target} is a different directory — press Enter to dispatch it there; Shift+Enter runs in {}",
                session.cwd
            ));
        }
        return;
    }
    // Enter dispatches into whichever project the view has selected. Running
    // here cannot follow that, so say which directory the work is actually
    // getting rather than letting the selection imply the other one.
    let view_scope_note = {
        let selected = agent_view_dispatch_cwd(app, session);
        let selected = selected.to_string_lossy().to_string();
        (!rebon_session::same_cwd(&selected, &session.cwd)).then(|| {
            format!(" Running in {} rather than the selected {selected} — Enter dispatches into the selected project instead.", session.cwd)
        })
    };
    if let Err(err) =
        crate::rebon_config::ensure_background_permission_mode_allowed(app.permission_mode)
    {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some(format!("failed to create attached background job: {err}"));
        }
        return;
    }

    let previous_session_id = session.session_id.clone();
    super::apply_new_session(app, session);
    if session.session_id == previous_session_id {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some("failed to create attached background session".to_string());
        }
        return;
    }

    let store = crate::background::cli_default_store();
    let runtime = background_runtime_from_session(
        session,
        session.ui_mode,
        app.effort_level,
        app.permission_mode,
    );
    let job = match crate::background::create_attached_background_job(
        &store,
        dispatch.prompt.clone(),
        images.clone(),
        PathBuf::from(&session.cwd),
        runtime,
        session.session_id.clone(),
        dispatch.agent_type.clone(),
    ) {
        Ok(job) => job,
        Err(err) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("failed to create attached background job: {err}"));
            }
            return;
        }
    };
    if crate::background::should_hide_agent_session_from_chats(dispatch.agent_type.as_deref()) {
        if let Err(error) = rebon_session::save_session_hidden_from_chats(
            &rebon_session::default_projects_root(),
            &session.cwd,
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
    session.attached_background_job_id = Some(job.job_id().to_string());

    app.agent_view = None;
    prepare_for_new_prompt_after_withdrawal(app, &mut session.engine_half.update_rx);
    // Be precise about where this runs. The job record exists so the work
    // shows up in agent view and can be detached later, but the turn itself
    // runs in this process — reading "background job" as "runs in a worker"
    // is the wrong conclusion to draw right before closing the terminal.
    super::inject_system_message(
        app,
        "info",
        &format!(
            "Running here, tracked as background job {}.{} /bg detaches it, /hosted moves it into a worker once this turn finishes. /agent-view returns to the list.",
            job.job_id(),
            view_scope_note.unwrap_or_default()
        ),
    );
    let mut submit = SubmitPayload {
        text: dispatch.prompt.clone(),
        model_text: None,
        user_message_uuid: None,
        image_pastes: images
            .iter()
            .map(|image| image.to_prompt_paste_content())
            .collect(),
        directory_attachments: Vec::new(),
        execution_policy: None,
        skill_invocations: Vec::new(),
    };
    commit_submit_payload_to_transcript(app, &mut submit, &session.session_id);
    super::repin_transcript_to_bottom(app);
    if let Some(admitted) = super::admit_active_prompt(
        app,
        session,
        handle,
        active_prompt.as_ref(),
        super::LocalTurnSource::AgentViewAttachment,
        submit,
    ) {
        *active_prompt = Some(admitted);
        app.is_loading = true;
    }
}

/// Dispatch a new agent into a worker and mirror it from this TUI.
///
/// The cheap half of `/hosted`: this session was never the one being handed
/// over, so there is no lock to give up and nothing to adopt — the job is
/// new, queued, and its worker is starting because it has work to do. All
/// that is left is waiting for its endpoint, which the same poll that
/// finishes a `/hosted` handover already does.
fn dispatch_agent_view_prompt_hosted(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    active_prompt: &Option<ActivePrompt>,
    prompt: String,
    images: Vec<rebon_types::PromptPasteContent>,
) {
    if session.pending_hosted_session.is_some() {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some("a handover is already in progress".to_string());
        }
        return;
    }
    // Mirroring means leaving this session behind on a fresh one, and a
    // running turn owns the session it is writing into. Swapping it out from
    // under the turn would strand it — the same reason Shift+Enter and
    // `/hosted` refuse mid-turn. Enter still dispatches without mirroring,
    // which touches nothing here.
    if active_prompt.is_some() || app.is_loading {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some(
                "cannot mirror a dispatch while a prompt is running — Enter dispatches without mirroring"
                    .to_string(),
            );
        }
        return;
    }
    let store = crate::background::cli_default_store();
    let runtime = background_runtime_from_session(
        session,
        session.ui_mode,
        app.effort_level,
        app.permission_mode,
    );
    let parent_job_id = dispatching_worker_job_id(session);
    let cwd = agent_view_dispatch_cwd(app, session);
    let (prompt, images) = prompt_from_paste_contents(prompt.trim().to_string(), images);
    let dispatch = crate::background::resolve_background_dispatch_prompt_with_skills(
        &prompt,
        &cwd,
        &session.engine_half.skill_registry.ids(),
    );
    let target_cwd = dispatch.cwd.clone().unwrap_or(cwd);
    // Resolution rewrote the prompt, so the paste references the images were
    // expanded against no longer line up — they are dropped, which is also
    // why the emptiness check has to come after this and not before it.
    let images = if dispatch.prompt == prompt {
        images
    } else {
        Vec::new()
    };
    if dispatch.prompt.trim().is_empty() && images.is_empty() {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some("background prompt is empty".to_string());
        }
        return;
    }
    let job = match crate::background::launch_background_prompt(
        &store,
        crate::background::BackgroundLaunchOptions {
            prompt: dispatch.prompt,
            images,
            cwd: target_cwd,
            isolate_in_worktree: true,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime,
            name: None,
            agent_type: dispatch.agent_type,
            parent_job_id,
        },
    ) {
        Ok(job) => job,
        Err(err) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("failed to start background job: {err}"));
            }
            return;
        }
    };

    // Leave this session behind on a fresh one, the same way an
    // attach-here dispatch does: the mirror is about to take over the
    // screen, and the current conversation stays on disk to resume.
    super::apply_new_session(app, session);
    session.attached_background_job_id = Some(job.job_id().to_string());
    session.pending_hosted_session = Some(crate::background::PendingHostedSession::dispatch(
        job.job_id().to_string(),
    ));
    app.agent_view = None;
    super::inject_local_command_feedback(
        app,
        "agents",
        &format!(
            "Started {} in a background worker — attaching to it. It keeps running when you close the terminal.",
            job.job_id()
        ),
    );
    app.follow_transcript_tail = true;
}

fn dispatch_agent_view_prompt(
    app: &mut AppState,
    session: &TuiEngineSession,
    prompt: String,
    images: Vec<rebon_types::PromptPasteContent>,
) {
    let store = crate::background::cli_default_store();
    dispatch_agent_view_prompt_in_store(app, session, &store, prompt, images);
}

fn dispatch_agent_view_prompt_in_store(
    app: &mut AppState,
    session: &TuiEngineSession,
    store: &crate::background::BackgroundStore,
    prompt: String,
    images: Vec<rebon_types::PromptPasteContent>,
) {
    let runtime = background_runtime_from_session(
        session,
        session.ui_mode,
        app.effort_level,
        app.permission_mode,
    );
    let parent_job_id = dispatching_worker_job_id(session);
    let cwd = agent_view_dispatch_cwd(app, session);
    let (prompt, images) = prompt_from_paste_contents(prompt.trim().to_string(), images);
    let dispatch = crate::background::resolve_background_dispatch_prompt_with_skills(
        &prompt,
        &cwd,
        &session.engine_half.skill_registry.ids(),
    );
    let target_cwd = dispatch.cwd.clone().unwrap_or(cwd);
    // Resolution rewrote the prompt, so the paste references the images were
    // expanded against no longer line up — they are dropped, which is also
    // why the emptiness check has to come after this and not before it.
    let images = if dispatch.prompt == prompt {
        images
    } else {
        Vec::new()
    };
    // Resolution eats the tokens it understands, so a prompt that was only
    // `@reviewer` or `@repo` arrives here empty. Launching it would start a
    // worker, a worktree and a provider call for an empty user message — the
    // hosted dispatch has refused that from the start; this one used to spend
    // the job to find out.
    if dispatch.prompt.trim().is_empty() && images.is_empty() {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some("background prompt is empty".to_string());
        }
        return;
    }
    match crate::background::launch_background_prompt(
        store,
        crate::background::BackgroundLaunchOptions {
            prompt: dispatch.prompt,
            images,
            cwd: target_cwd,
            isolate_in_worktree: true,
            require_worktree: false,
            preserve_worktree_on_success: false,
            queue_session: false,
            runtime,
            name: None,
            agent_type: dispatch.agent_type,
            parent_job_id,
        },
    ) {
        Ok(job) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("started background job {}", job.job_id()));
            }
        }
        Err(err) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("failed to start background job: {err}"));
            }
        }
    }
}

pub(super) fn background_attach_recap_message(
    job_id: &str,
    name: &str,
    status: crate::background::BackgroundJobStatus,
    summary: Option<&str>,
) -> String {
    let summary = summary
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .unwrap_or("No summary was recorded while this job was detached.");
    format!(
        "Attached background job {job_id} ({name}). Recap from agent view: {} — {summary}",
        status.as_str()
    )
}

/// One line for "stopped X, and the N things X had started".
fn stopped_tree_status(job_id: &str, tree: &crate::background::StoppedJobTree) -> String {
    let mut status = format!("stopped {job_id}");
    if !tree.stopped_children.is_empty() {
        status.push_str(&format!(
            " and {} it started",
            match tree.stopped_children.len() {
                1 => "the agent".to_string(),
                count => format!("the {count} agents"),
            }
        ));
    }
    for (child, error) in &tree.failed_children {
        status.push_str(&format!("; could not stop {child}: {error}"));
    }
    status
}

/// Stop every job in one Agent View group and remove it. The first
/// failure is the one reported; the rest of the group is still tried, so
/// one wedged job does not strand the others.
fn remove_agent_view_group(app: &mut AppState, group_label: &str, job_ids: Vec<String>) {
    let store = crate::background::cli_default_store();
    let mut removed = 0usize;
    let mut first_error = None;
    for job_id in job_ids {
        if let Err(err) = stop_agent_view_job_in_store(&store, &job_id) {
            if first_error.is_none() {
                first_error = Some(format!("{job_id}: {err}"));
            }
            continue;
        }
        match store.remove_job(&job_id) {
            Ok(()) => removed += 1,
            Err(err) if first_error.is_none() => {
                first_error = Some(format!("{job_id}: {err}"));
            }
            Err(_) => {}
        }
    }
    if let Some(view) = app.agent_view.as_mut() {
        view.status = Some(match first_error {
            Some(err) => {
                format!("removed {removed} job(s) from {group_label}; first error: {err}")
            }
            None => format!("removed {removed} job(s) from {group_label}"),
        });
    }
    refresh_agent_view(app);
}

/// Attach this terminal to a background job's session. A running prompt
/// owns the transcript, so the attach is refused rather than queued.
fn attach_agent_view_job(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    active_prompt: &mut Option<ActivePrompt>,
    job_id: &str,
) {
    if active_prompt.is_some() || app.is_loading {
        if let Some(view) = app.agent_view.as_mut() {
            view.status =
                Some("cannot attach another session while a prompt is running".to_string());
        }
        return;
    }
    match crate::background::attach_background_job(job_id) {
        Ok(target) => {
            if super::apply_background_attach_target(app, session, &target) {
                app.agent_view = None;
                session.attached_background_job_id = Some(target.job_id.clone());
                let recap = background_attach_recap_message(
                    &target.job_id,
                    &target.name,
                    target.status,
                    target.summary.as_deref(),
                );
                super::inject_system_message(app, "info", &recap);
                app.follow_transcript_tail = true;
            }
        }
        Err(err) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("failed to attach {job_id}: {err}"));
            }
        }
    }
}

/// Stop one task from Agent View. A task this terminal only mirrors is
/// cancelled through the job that owns it; a local one is stopped here
/// and its escalations are cancelled with it.
fn stop_agent_view_task(app: &mut AppState, session: &mut TuiEngineSession, task_id: &str) {
    let remote_job_id = session
        .remote_background_attachment
        .as_ref()
        .filter(|_| app.remote_background_tasks.contains_key(task_id))
        .map(|remote| remote.job_id.clone());
    let stopped = if let Some(job_id) = remote_job_id {
        crate::background::cancel_background_job_tasks(&job_id, vec![task_id.to_string()])
            .map(|_| ())
            .map_err(|err| err.to_string())
    } else {
        let id = rebon_plugin_tasks::runtime::TaskId::new(task_id.to_string());
        rebon_plugin_tasks::runtime::stop_task(session.engine_half.tasks.as_ref(), &id)
            .map(|_| {
                session
                    .engine_half
                    .tasks
                    .escalation_registry()
                    .cancel_agent(task_id, "agent was stopped from Agent View");
            })
            .map_err(|err| err.to_string())
    };
    report_agent_view_action(
        app,
        stopped
            .map(|()| format!("stopped {task_id}"))
            .map_err(|err| format!("failed to stop {task_id}: {err}")),
    );
}

/// Remove one task from Agent View: stop it first (a task that was never
/// running is already stopped), then drop it from the registry.
fn remove_agent_view_task(app: &mut AppState, session: &mut TuiEngineSession, task_id: &str) {
    let id = rebon_plugin_tasks::runtime::TaskId::new(task_id.to_string());
    match rebon_plugin_tasks::runtime::stop_task(session.engine_half.tasks.as_ref(), &id) {
        Ok(_) | Err(rebon_plugin_tasks::runtime::StopTaskError::NotRunning(_, _)) => {}
        Err(err) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("failed to remove {task_id}: {err}"));
            }
            return;
        }
    }
    session
        .engine_half
        .tasks
        .escalation_registry()
        .cancel_agent(task_id, "agent was removed from Agent View");
    if session.engine_half.tasks.remove(&id).is_some() {
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some(format!("removed {task_id}"));
        }
        refresh_agent_view(app);
    } else if let Some(view) = app.agent_view.as_mut() {
        view.status = Some(format!("failed to remove {task_id}: task not found"));
    }
}

/// A reply typed into Agent View for a task. Three things can be meant
/// by it: approving a plan the task is waiting on, an `/interrupt`
/// redirect that replaces the agent with one given a new instruction,
/// and otherwise a message sent to the agent as it runs.
fn reply_to_agent_view_task(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    task_id: String,
    message: String,
) {
    let maybe_snapshot = session
        .engine_half
        .tasks
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.id.to_string() == task_id);
    if matches!(
        maybe_snapshot.as_ref().map(|snapshot| &snapshot.data),
        Some(rebon_plugin_tasks::runtime::TaskData::InProcessTeammate(data))
            if data.awaiting_plan_approval
    ) {
        let payload = serde_json::json!({
            "type": "plan_approval_response",
            "requestId": "agent-view",
            "approved": true,
            "feedback": message,
            "permissionMode": app.permission_mode.as_wire(),
        })
        .to_string();
        let ok = rebon_plugin_tasks::runtime::inject_user_message_to_teammate(
            session.engine_half.tasks.as_ref(),
            &rebon_plugin_tasks::runtime::TaskId::new(task_id.clone()),
            payload,
        );
        if let Some(view) = app.agent_view.as_mut() {
            view.status = Some(if ok {
                format!("approved task plan for {task_id}")
            } else {
                format!("failed to answer task option for {task_id}")
            });
        }
        refresh_agent_view(app);
        return;
    }
    if !app.is_remote_agent_task(&task_id) {
        if let Some(new_instruction) = super::parse_agent_interrupt_redirect(&message) {
            let result = if new_instruction.trim().is_empty() {
                Err(
                    "`/interrupt` requires a new instruction for the replacement agent."
                        .to_string(),
                )
            } else {
                super::interrupt_and_continue_local_agent_task(
                    session,
                    handle,
                    &task_id,
                    &new_instruction,
                    true,
                )
                .map(|new_task_id| format!("interrupted {task_id}; continued as {new_task_id}"))
            };
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(match result {
                    Ok(status) => status,
                    Err(err) => err,
                });
            }
            refresh_agent_view(app);
            return;
        }
    }
    match super::foreground_agent_submit::send_message_to_agent_task(
        app, session, handle, &task_id, message,
    ) {
        Ok(()) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(format!("sent reply to {task_id}"));
            }
        }
        Err(err) => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(err);
            }
        }
    }
}

/// Report one Agent View action on the panel's status line. A success
/// also refreshes the panel, because the rows it lists have just
/// changed; a failure leaves them alone, so the message stays next to
/// the row it is about.
fn report_agent_view_action(app: &mut AppState, outcome: Result<String, String>) {
    let succeeded = outcome.is_ok();
    if let Some(view) = app.agent_view.as_mut() {
        view.status = Some(match outcome {
            Ok(status) | Err(status) => status,
        });
    }
    if succeeded {
        refresh_agent_view(app);
    }
}

pub(super) fn handle_agent_view_outcome(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    active_prompt: &mut Option<ActivePrompt>,
    outcome: AgentViewKeyOutcome,
) -> bool {
    match outcome {
        AgentViewKeyOutcome::CancelOrExit => false,
        AgentViewKeyOutcome::ReturnToSession => {
            app.agent_view = None;
            true
        }
        AgentViewKeyOutcome::Dismiss => {
            app.agent_view = None;
            if active_prompt.is_some() || !app.rebon_tui.transcript.is_empty() {
                background_current_session(app, session, None, active_prompt);
            }
            true
        }
        AgentViewKeyOutcome::DispatchPrompt(prompt) => {
            dispatch_agent_view_prompt(app, session, prompt, Vec::new());
            true
        }
        AgentViewKeyOutcome::DispatchPromptWithImages { prompt, images } => {
            dispatch_agent_view_prompt(app, session, prompt, images);
            true
        }
        AgentViewKeyOutcome::DispatchPromptAndAttach(prompt) => {
            dispatch_agent_view_prompt_and_attach(
                app,
                session,
                handle,
                active_prompt,
                prompt,
                Vec::new(),
            );
            true
        }
        AgentViewKeyOutcome::DispatchPromptAndAttachWithImages { prompt, images } => {
            dispatch_agent_view_prompt_and_attach(
                app,
                session,
                handle,
                active_prompt,
                prompt,
                images,
            );
            true
        }
        AgentViewKeyOutcome::DispatchPromptHosted(prompt) => {
            dispatch_agent_view_prompt_hosted(app, session, active_prompt, prompt, Vec::new());
            true
        }
        AgentViewKeyOutcome::DispatchPromptHostedWithImages { prompt, images } => {
            dispatch_agent_view_prompt_hosted(app, session, active_prompt, prompt, images);
            true
        }
        AgentViewKeyOutcome::ReplyToTask { task_id, message } => {
            reply_to_agent_view_task(app, session, handle, task_id, message);
            true
        }
        AgentViewKeyOutcome::ReplyToJob { job_id, message } => {
            report_agent_view_action(
                app,
                crate::background::reply_to_background_job(&job_id, message)
                    .map(|()| format!("sent reply to {job_id}"))
                    .map_err(|err| format!("failed to send reply to {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::BashCommandToJob { job_id, command } => {
            // The engine recognises a leading `!` in a user prompt as a
            // Bash directive. Re-prepend the prefix so the supervised
            // session's prompt processor runs the command rather than
            // routing it through the LLM as plain text.
            let payload = format!("!{command}");
            report_agent_view_action(
                app,
                crate::background::reply_to_background_job(&job_id, payload)
                    .map(|()| format!("ran bash in {job_id}: {command}"))
                    .map_err(|err| format!("failed to run bash in {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::BashCommandToTask { task_id, command } => {
            let payload = format!("!{command}");
            match super::foreground_agent_submit::send_message_to_agent_task(
                app, session, handle, &task_id, payload,
            ) {
                Ok(()) => {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.status = Some(format!("ran bash in {task_id}: {command}"));
                    }
                }
                Err(err) => {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.status = Some(format!("failed to run bash in {task_id}: {err}"));
                    }
                }
            }
            true
        }
        AgentViewKeyOutcome::AnswerJobPermission {
            job_id,
            query_id,
            turn_generation,
            endpoint,
            option_id,
        } => {
            let store = crate::background::cli_default_store();
            report_agent_view_action(
                app,
                store
                    .answer_permission_query_for_target_with_updated_input(
                        &job_id,
                        query_id,
                        Some(turn_generation),
                        endpoint.as_ref(),
                        Some(option_id),
                        None,
                        None,
                    )
                    .map(|()| format!("answered permission for {job_id}"))
                    .map_err(|err| format!("failed to answer permission for {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::AnswerTaskChoice {
            task_id,
            option_index,
        } => {
            let approved = option_index == 0;
            let message = serde_json::json!({
                "type": "plan_approval_response",
                "requestId": "agent-view",
                "approved": approved,
                "feedback": if approved { "Approved from Agent View." } else { "Please revise the plan." },
                "permissionMode": app.permission_mode.as_wire(),
            })
            .to_string();
            let result = super::foreground_agent_submit::send_message_to_agent_task(
                app, session, handle, &task_id, message,
            );
            if let Some(view) = app.agent_view.as_mut() {
                view.status = Some(match result {
                    Ok(()) => format!("answered task option {} for {task_id}", option_index + 1),
                    Err(err) => format!("failed to answer task option for {task_id}: {err}"),
                });
            }
            refresh_agent_view(app);
            true
        }
        AgentViewKeyOutcome::RenameJob { job_id, name } => {
            let store = crate::background::cli_default_store();
            report_agent_view_action(
                app,
                store
                    .update_job_presentation(&job_id, Some(name), None, None)
                    .map(|job| format!("renamed {job_id} to {}", job.name()))
                    .map_err(|err| format!("failed to rename {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::TogglePinJob { job_id } => {
            let store = crate::background::cli_default_store();
            report_agent_view_action(
                app,
                store
                    .read_state(&job_id)
                    .and_then(|job| {
                        store.update_job_presentation(&job_id, None, Some(!job.pinned()), None)
                    })
                    .map(|job| {
                        let status = if job.pinned() { "pinned" } else { "unpinned" };
                        format!("{status} {job_id}")
                    })
                    .map_err(|err| format!("failed to pin {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::MoveJobUp { job_id } => {
            move_agent_view_job(app, &job_id, true);
            true
        }
        AgentViewKeyOutcome::MoveJobDown { job_id } => {
            move_agent_view_job(app, &job_id, false);
            true
        }
        AgentViewKeyOutcome::RemoveGroup {
            group_label,
            job_ids,
        } => {
            remove_agent_view_group(app, &group_label, job_ids);
            true
        }
        AgentViewKeyOutcome::OpenTask { task_id } => {
            app.agent_view = None;
            let mut detached_prompt = None;
            super::switch_to_live_agent(app, &mut detached_prompt, &task_id);
            true
        }
        AgentViewKeyOutcome::RefreshPeek => {
            refresh_agent_view(app);
            true
        }
        AgentViewKeyOutcome::WarmPeekJob { job_id } => {
            match crate::background::warm_background_job_for_peek(&job_id) {
                Ok(true) => {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.status = Some(format!("peek warming {job_id}"));
                    }
                    refresh_agent_view(app);
                }
                Ok(false) => {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.status = Some("peek panel shown".to_string());
                    }
                    refresh_agent_view(app);
                }
                Err(err) => {
                    if let Some(view) = app.agent_view.as_mut() {
                        view.status = Some(format!("failed to warm {job_id} for peek: {err}"));
                    }
                    refresh_agent_view(app);
                }
            }
            true
        }
        AgentViewKeyOutcome::AttachJob { job_id } => {
            attach_agent_view_job(app, session, active_prompt, &job_id);
            true
        }
        AgentViewKeyOutcome::StopJob { job_id } => {
            let store = crate::background::cli_default_store();
            report_agent_view_action(
                app,
                stop_agent_view_job_in_store(&store, &job_id)
                    .map(|tree| stopped_tree_status(&job_id, &tree))
                    .map_err(|err| format!("failed to stop {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::StopTask { task_id } => {
            stop_agent_view_task(app, session, &task_id);
            true
        }
        AgentViewKeyOutcome::RespawnJob { job_id } => {
            report_agent_view_action(
                app,
                crate::background::respawn_background_job(&job_id)
                    .map(|job| format!("respawned {job_id} as {}", job.job_id()))
                    .map_err(|err| format!("failed to respawn {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::RemoveJob { job_id } => {
            report_agent_view_action(
                app,
                crate::background::remove_background_job(&job_id)
                    .map(|()| format!("removed {job_id}"))
                    .map_err(|err| format!("failed to remove {job_id}: {err}")),
            );
            true
        }
        AgentViewKeyOutcome::RemoveTask { task_id } => {
            remove_agent_view_task(app, session, &task_id);
            true
        }
        AgentViewKeyOutcome::GroupingChanged(grouping) => {
            persist_agent_view_grouping_preference(grouping);
            true
        }
        AgentViewKeyOutcome::FilterChanged => {
            refresh_agent_view(app);
            true
        }
        AgentViewKeyOutcome::EditInputExternally { .. } => {
            if let Some(view) = app.agent_view.as_mut() {
                view.status =
                    Some("external editor is only available from Agent View input".to_string());
            }
            true
        }
        AgentViewKeyOutcome::Consumed => true,
    }
}

#[cfg(test)]
mod hosted_handover_tests {
    use super::*;
    use crate::tui::app::AppState;

    fn transcript_mentions(app: &AppState, needle: &str) -> bool {
        app.rebon_tui.transcript.rows().iter().any(|row| {
            matches!(row, rebon_tui::Message::System(message)
                if message.content.as_deref().is_some_and(|c| c.contains(needle)))
        })
    }

    fn unique_job_id(label: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("bg-hosted-{label}-{}-{nonce}", std::process::id())
    }

    /// Handing a session over mid-turn would leave two processes owning one
    /// session — exactly what the attach path refuses elsewhere. Refuse it
    /// here too, before anything is written to the job store.
    #[test]
    fn hosted_refuses_while_a_local_turn_is_running() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));

        host_current_session_in_worker(&mut app, &mut session, &active);

        assert!(transcript_mentions(
            &app,
            "Finish or cancel the running turn"
        ));
        assert!(session.pending_hosted_session.is_none());
        assert!(session.attached_background_job_id.is_none());
    }

    /// RFC-0004 I6, the other gestures: `/bg` (with or without a final
    /// prompt) and Ctrl+Z on a turn running in this process are refused
    /// the same way, and the turn stays exactly where it is. The
    /// in-process detach that used to carry the turn into a job record is
    /// gone — a hosted session's turn already runs in its worker, and a
    /// `--local` one ends first.
    #[test]
    fn a_running_local_turn_is_not_backgrounded_by_bg_or_ctrl_z() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let cancel = rebon_types::PromptCancel::new();
        let mut active = Some(ActivePrompt::new(rx, cancel.clone()));
        let rows_before = app.rebon_tui.transcript.len();

        background_current_session(
            &mut app,
            &mut session,
            Some("finish up afterwards".into()),
            &mut active,
        );

        assert!(active.is_some(), "the turn is still this process's");
        assert!(!cancel.is_cancelled());
        assert!(session.attached_background_job_id.is_none());
        assert!(session.remote_background_attachment.is_none());
        assert!(app.agent_view.is_none(), "nothing to go to the list for");
        assert!(transcript_mentions(
            &app,
            "Finish or cancel the running turn"
        ));
        assert_eq!(app.rebon_tui.transcript.len(), rows_before + 1);

        // Ctrl+Z on an attach-here session (a job record, the turn in this
        // process): same refusal, the job stays the session's.
        session.attached_background_job_id = Some(unique_job_id("attach-here"));
        let job_id = session.attached_background_job_id.clone();
        background_current_session(&mut app, &mut session, None, &mut active);
        assert!(active.is_some());
        assert_eq!(session.attached_background_job_id, job_id);
        assert!(app.agent_view.is_none());
    }

    /// The handover gives the session's active lock to the worker, because
    /// the worker cannot resume a session this process still claims. A
    /// handover that never starts must not give it away — the session is
    /// still being used right here.
    #[test]
    fn a_refused_handover_keeps_the_session_lock() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.session_id = unique_job_id("lock-session");
        session.session_active_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .expect("lock probe")
        .or_else(|| panic!("test session should be lockable"));
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));

        host_current_session_in_worker(&mut app, &mut session, &active);

        assert!(
            session.session_active_lock.is_some(),
            "a handover that was refused must not hand the session away"
        );
    }

    #[test]
    fn hosted_is_a_no_op_once_the_session_already_runs_in_a_worker() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                unique_job_id("already"),
                "sess-hosted".into(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "hosted-token".into(),
                },
            ));

        host_current_session_in_worker(&mut app, &mut session, &None);

        assert!(transcript_mentions(
            &app,
            "already runs in a background worker"
        ));
        assert!(session.pending_hosted_session.is_none());
    }

    /// The first second of a wait probes densely — the worker is expected
    /// any moment — and settles to the slower cadence afterwards.
    #[test]
    fn the_wait_probes_densely_for_its_first_second() {
        let fresh = crate::background::PendingHostedSession::startup("bg-fresh".into());
        assert_eq!(hosted_probe_interval(&fresh), HOSTED_PROBE_INTERVAL_EAGER);

        let mut old = crate::background::PendingHostedSession::startup("bg-old".into());
        old.started_at = std::time::Instant::now()
            .checked_sub(HOSTED_PROBE_EAGER_FOR * 2)
            .unwrap_or_else(std::time::Instant::now);
        assert_eq!(hosted_probe_interval(&old), HOSTED_PROBE_INTERVAL);

        // The dense cadence is what the poll actually uses: a probe 30 ms
        // after the last one runs while the wait is young, and would not
        // under the settled cadence.
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Succeeded,
            Some("sess-dense"),
            None,
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut pending = crate::background::PendingHostedSession::dispatch(job_id.clone());
        pending.last_probe_at = Some(
            std::time::Instant::now()
                .checked_sub(HOSTED_PROBE_INTERVAL_EAGER + std::time::Duration::from_millis(5))
                .unwrap_or_else(std::time::Instant::now),
        );
        wait_for(&mut session, pending);
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert!(
            session.pending_hosted_session.is_none(),
            "the young wait probed and saw the job was over"
        );
    }

    /// A worker that never comes up must not leave the session spinning
    /// forever: the handover already happened, so the message has to say
    /// the wait is over and what to do next.
    #[test]
    fn a_handover_whose_worker_never_appears_fails_loudly_and_parks_the_session() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let job_id = unique_job_id("never-appears");
        session.attached_background_job_id = Some(job_id.clone());
        session.pending_hosted_session = Some(crate::background::PendingHostedSession {
            job_id: job_id.clone(),
            kind: crate::background::PendingHostedKind::Handover,
            started_at: std::time::Instant::now()
                .checked_sub(HOSTED_WAIT_GIVE_UP_AFTER * 2)
                .unwrap_or_else(std::time::Instant::now),
            last_probe_at: None,
            slow_notice_shown: false,
        });
        let mut pending_permission = None;

        poll_pending_hosted_session(&mut app, &mut session, &mut pending_permission);

        assert!(transcript_mentions(&app, "did not come up"));
        assert!(transcript_mentions(&app, "Type to continue"));
        assert!(session.pending_hosted_session.is_none());
        // Giving up the wait is not getting the session back. The worker is
        // late, not gone: it can still come up and resume this session, and
        // the lock the handover released is not reclaimed here. The session
        // stays the job's, parked: the next prompt gives the job a worker,
        // and a late one is followed when it turns up.
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some(job_id.as_str()),
            "the session is still the job's"
        );
        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("parked on the job");
        assert!(!remote.is_live(), "there is no worker to follow");
        assert_eq!(remote.job_id, job_id);
        assert!(session.session_active_lock.is_none());
    }

    /// The default startup waits the way a handover does and says less:
    /// nothing while the worker is coming up — the status bar's dot is the
    /// signal — and past thirty seconds one notice, once, naming `--local`.
    #[test]
    fn a_startup_wait_is_silent_and_then_says_so_once() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let job_id = unique_job_id("startup-slow");
        wait_for(
            &mut session,
            crate::background::PendingHostedSession::startup(job_id.clone()),
        );
        let mut pending_permission = None;

        poll_pending_hosted_session(&mut app, &mut session, &mut pending_permission);
        assert!(
            app.rebon_tui.transcript.is_empty(),
            "a young wait says nothing"
        );

        let pending = session.pending_hosted_session.as_mut().unwrap();
        pending.started_at = std::time::Instant::now()
            .checked_sub(HOSTED_STARTUP_SLOW_NOTICE + std::time::Duration::from_secs(1))
            .unwrap_or_else(std::time::Instant::now);
        pending.last_probe_at = None;
        poll_pending_hosted_session(&mut app, &mut session, &mut pending_permission);
        assert!(transcript_mentions(&app, "Still starting the session host"));
        assert!(transcript_mentions(&app, "--local"));
        let rows_after_notice = app.rebon_tui.transcript.len();

        session
            .pending_hosted_session
            .as_mut()
            .unwrap()
            .last_probe_at = None;
        poll_pending_hosted_session(&mut app, &mut session, &mut pending_permission);
        assert_eq!(
            app.rebon_tui.transcript.len(),
            rows_after_notice,
            "said once"
        );
        assert!(
            session.pending_hosted_session.is_some(),
            "still waiting: slow is not failed"
        );
    }

    /// The first worker of a brand-new session died before it hosted
    /// anything. The session parks on the job like any other, and the
    /// notice names both ways out: another worker, or this process.
    #[test]
    fn a_startup_whose_worker_died_parks_the_session_and_names_local() {
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Failed,
            Some("sess-startup-died"),
            Some("boom"),
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let projects = tempfile::tempdir().unwrap();
        session.projects_root = projects.path().to_path_buf();
        session.session_id = unique_job_id("startup-died");
        wait_for(
            &mut session,
            crate::background::PendingHostedSession::startup(job_id.clone()),
        );
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert!(transcript_mentions(&app, "failed"));
        assert!(transcript_mentions(&app, "boom"));
        assert!(transcript_mentions(&app, "Type to continue"));
        assert!(transcript_mentions(&app, "--local"));
        assert_parked_on(&session, &job_id);
    }

    /// Inside the budget the poll is patient: the worker not being up yet
    /// (or refusing an attach once) is not a reason to give up — no message,
    /// and the handover stays pending until the budget says otherwise.
    #[test]
    fn a_handover_still_within_budget_keeps_waiting_quietly() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let job_id = unique_job_id("still-waiting");
        session.attached_background_job_id = Some(job_id.clone());
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
            job_id.clone(),
        ));
        let mut pending_permission = None;

        poll_pending_hosted_session(&mut app, &mut session, &mut pending_permission);

        assert!(app.rebon_tui.transcript.is_empty());
        assert_eq!(
            session
                .pending_hosted_session
                .as_ref()
                .map(|pending| pending.job_id.clone()),
            Some(job_id)
        );
    }

    fn hosted_test_runtime() -> crate::background::BackgroundRuntimeFields {
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
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
        }
    }

    /// A job in a private store, already in whatever state the test is about.
    fn store_with_job(
        status: crate::background::BackgroundJobStatus,
        session_id: Option<&str>,
        error: Option<&str>,
    ) -> (
        tempfile::TempDir,
        crate::background::BackgroundStore,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut job = store
            .create_job(
                "dispatched agent".into(),
                std::path::PathBuf::from("."),
                hosted_test_runtime(),
            )
            .unwrap();
        job.process.status = status;
        job.identity.session_id = session_id.map(str::to_string);
        job.outcome.error = error.map(str::to_string);
        job.process.completed_at_ms = Some(rebon_session_host::now_ms());
        store.write_state(&job).unwrap();
        let job_id = job.job_id().to_string();
        (dir, store, job_id)
    }

    fn wait_for(session: &mut TuiEngineSession, pending: crate::background::PendingHostedSession) {
        session.attached_background_job_id = Some(pending.job_id.clone());
        session.pending_hosted_session = Some(pending);
    }

    /// Ownership is recorded only when the dispatch really comes from a
    /// worker. A session running its turns in this process has no worker to
    /// belong to — writing this terminal's job id there would promise a
    /// release that nothing can perform.
    #[test]
    fn only_a_session_that_runs_in_a_worker_owns_what_it_dispatches() {
        let mut session = super::super::test_support::make_test_tui_session();
        assert_eq!(dispatching_worker_job_id(&session), None);

        // Attach-here records a job id, but the turn runs locally.
        session.attached_background_job_id = Some("bg-runs-here".into());
        assert_eq!(dispatching_worker_job_id(&session), None);

        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-the-worker".into(),
                "sess-mirrored".into(),
                session.cwd.clone(),
                crate::background::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "t".into(),
                },
            ));
        assert_eq!(
            dispatching_worker_job_id(&session).as_deref(),
            Some("bg-the-worker")
        );
    }

    /// The gesture: two Lefts on an empty prompt route into hosting. Proved
    /// through the refusal path on purpose — it reaches the hosting code
    /// without creating a job in the user's real store.
    #[test]
    fn two_lefts_on_an_empty_prompt_reach_the_handover() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));

        assert!(
            !host_session_on_double_left(&mut app, &mut session, &active),
            "one Left is still cursor movement"
        );
        assert!(app.last_left_press_ms > 0);

        assert!(host_session_on_double_left(&mut app, &mut session, &active));
        assert!(transcript_mentions(
            &app,
            "Finish or cancel the running turn"
        ));
        assert_eq!(app.last_left_press_ms, 0, "the gesture is consumed");
    }

    /// With text in the prompt, `←` is doing its day job — moving through
    /// what the user typed. A gesture that stole it would be unusable.
    #[test]
    fn lefts_with_text_in_the_prompt_never_become_the_gesture() {
        let mut app = AppState::new();
        app.input = "explain this".into();
        let mut session = super::super::test_support::make_test_tui_session();

        assert!(!host_session_on_double_left(&mut app, &mut session, &None));
        assert!(!host_session_on_double_left(&mut app, &mut session, &None));
        assert_eq!(app.last_left_press_ms, 0);
        assert!(app.rebon_tui.transcript.is_empty());
    }

    /// Two presses a minute apart are two presses, not a gesture.
    #[test]
    fn a_left_outside_the_double_press_window_is_just_a_left() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        app.last_left_press_ms =
            super::super::status_bar::wall_clock_ms() - DOUBLE_PRESS_WINDOW_MS * 10;

        assert!(!host_session_on_double_left(&mut app, &mut session, &None));
        assert!(app.rebon_tui.transcript.is_empty());
    }

    /// The second thing a real machine found: `rebon --hosted` hands over a
    /// conversation nobody has spoken in yet, and a session's transcript file
    /// is only written on the first append — so the worker started, tried to
    /// resume, and failed with "Session not found" a second later.
    #[test]
    fn handing_over_a_session_nobody_has_spoken_in_yet_creates_it_on_disk_first() {
        let mut session = super::super::test_support::make_test_tui_session();
        let projects = tempfile::tempdir().unwrap();
        session.projects_root = projects.path().to_path_buf();
        session.session_id = unique_job_id("never-spoken");
        let path = rebon_session::transcript_file_path(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        );
        assert!(!path.exists(), "a fresh session has no transcript yet");

        session
            .ensure_transcript_on_disk()
            .expect("materialise transcript");

        assert!(
            path.exists(),
            "a worker resumes from disk, so the session has to be there to resume"
        );
        // And it is the session, not a placeholder: loading it yields an empty
        // conversation rather than "not found".
        let loaded = rebon_session::load_raw_transcript_from_file(&path).unwrap();
        assert!(loaded.is_some_and(|file| file.entries.is_empty()));
    }

    /// The bug a real machine found, and the reason the wait is read-only.
    ///
    /// `/hosted` warms the job by queueing it, and the supervisor spawns the
    /// worker on its next tick — so for that first second the job is `Queued`
    /// with a session id and no owner. The wait used to ask
    /// `attach_background_job`, which is the *takeover* path: an ownerless
    /// job gets released, which stamps it `Stopped`. The supervisor only ever
    /// spawns `Queued`, so 17ms after the handover started, the worker it was
    /// waiting for could no longer come.
    #[test]
    fn a_queued_handover_is_not_stopped_by_the_poll_that_waits_for_it() {
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Queued,
            Some("sess-queued-handover"),
            None,
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        wait_for(
            &mut session,
            crate::background::PendingHostedSession::handover(job_id.clone()),
        );
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert_eq!(
            store.read_state(&job_id).unwrap().status(),
            crate::background::BackgroundJobStatus::Queued,
            "waiting for a worker must not stop the job the supervisor is about to pick up"
        );
        assert!(session.pending_hosted_session.is_some());
        assert!(app.rebon_tui.transcript.is_empty());
    }

    /// The point of Ctrl+N is a job that outlives this terminal, so a job that
    /// finished before the mirror arrived is not a failure — but the wait was
    /// written for a handover and would spend the full budget only to report
    /// that the worker "did not come up". It came up, ran, and left.
    #[test]
    fn a_dispatch_whose_job_already_finished_says_so_instead_of_waiting_out_the_budget() {
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Succeeded,
            Some("sess-finished"),
            None,
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        wait_for(
            &mut session,
            crate::background::PendingHostedSession::dispatch(job_id.clone()),
        );
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert!(session.pending_hosted_session.is_none());
        assert!(transcript_mentions(&app, "finished"));
        assert!(transcript_mentions(&app, &job_id));
        assert!(
            !transcript_mentions(&app, "did not come up"),
            "a job that ran must not be reported as a worker that never started"
        );
    }

    /// A job that dies before it builds a session answers every attach with
    /// "has not started a session yet" — forever. The status, not the error,
    /// is what says the wait is pointless, and the recorded reason is the only
    /// thing that explains it.
    #[test]
    fn a_dispatch_that_failed_before_its_session_reports_the_recorded_reason() {
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Failed,
            None,
            Some("worktree preparation failed: not a git repository"),
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        wait_for(
            &mut session,
            crate::background::PendingHostedSession::dispatch(job_id.clone()),
        );
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert!(session.pending_hosted_session.is_none());
        assert!(session.attached_background_job_id.is_none());
        assert!(transcript_mentions(&app, "failed"));
        assert!(transcript_mentions(&app, "not a git repository"));
    }

    /// The parked shape every "the worker is gone" path has to leave: the
    /// session on screen is still the job's, with nothing to follow, and
    /// this process holds no claim on it.
    fn assert_parked_on(session: &TuiEngineSession, job_id: &str) {
        assert!(session.pending_hosted_session.is_none());
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some(job_id),
            "the session is still the job's"
        );
        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("parked on the job");
        assert_eq!(remote.job_id, job_id);
        assert!(!remote.is_live(), "there is no worker to follow");
        assert!(
            session.session_active_lock.is_none(),
            "a handed-over session is never taken back into this process"
        );
    }

    /// Same early exit, opposite consequence: the handover gave this
    /// session away, and it does not come back to this process — not even
    /// with the worker over. The session is parked on the job, and the
    /// message says a prompt is what gives it another worker.
    #[test]
    fn a_handover_whose_worker_ended_early_parks_the_session_on_the_job() {
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Stopped,
            Some("sess-handed-over"),
            None,
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let projects = tempfile::tempdir().unwrap();
        session.projects_root = projects.path().to_path_buf();
        session.session_id = unique_job_id("ended-early");
        wait_for(
            &mut session,
            crate::background::PendingHostedSession::handover(job_id.clone()),
        );
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert!(transcript_mentions(&app, "was stopped"));
        assert!(transcript_mentions(&app, "Type to continue"));
        assert_parked_on(&session, &job_id);
        assert_eq!(
            session
                .remote_background_attachment
                .as_ref()
                .map(|remote| remote.status),
            Some(crate::background::BackgroundJobStatus::Stopped)
        );
    }

    /// `/stop` during a handover ends the wait, but the session went with
    /// the worker and stays with the job: it is parked, whether or not
    /// something else still holds its claim. Left half-done, the session
    /// had no lock and nothing refusing local turns on it — the two-writer
    /// state again.
    #[test]
    fn stopping_a_job_mid_handover_parks_the_session_on_it() {
        for competing_claim in [true, false] {
            let (_dir, store, job_id) = store_with_job(
                crate::background::BackgroundJobStatus::Queued,
                Some("sess-stop-mid-handover"),
                None,
            );
            let mut app = AppState::new();
            let mut session = super::super::test_support::make_test_tui_session();
            let projects = tempfile::tempdir().unwrap();
            session.projects_root = projects.path().to_path_buf();
            session.session_id = unique_job_id("stop-mid-handover");
            // As `/hosted` leaves it: lock released, waiting for the worker.
            session.attached_background_job_id = Some(job_id.clone());
            wait_for(
                &mut session,
                crate::background::PendingHostedSession::handover(job_id.clone()),
            );
            // Something else may hold the claim — the late worker, say.
            let _competing_lock = competing_claim.then(|| {
                rebon_session::try_acquire_session_active_lock(
                    &session.projects_root,
                    &session.cwd,
                    &session.session_id,
                )
                .unwrap()
                .expect("the competing claim is acquired")
            });

            let stopped =
                stop_attached_background_session_in_store(&mut app, &mut session, &store).unwrap();

            assert!(
                stopped.feedback.contains(&job_id),
                "the sentence says which worker was stopped: {}",
                stopped.feedback
            );
            assert!(
                stopped.feedback.contains("Type to continue"),
                "{}",
                stopped.feedback
            );
            assert_parked_on(&session, &job_id);
            assert!(
                app.agent_view.is_none(),
                "the session stays on screen; the list is a Ctrl+Z away"
            );
        }
    }

    /// The same early exit when the claim does NOT come back: something
    /// else holds this session's active lock, which means something else
    /// can still write its transcript. Reopening local submission here is
    /// the two-owner state the handover exists to prevent, so the session
    /// is parked on the job rather than made this terminal's again.
    #[test]
    fn a_handover_that_cannot_reclaim_its_session_parks_it() {
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Stopped,
            Some("sess-still-claimed"),
            None,
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let projects = tempfile::tempdir().unwrap();
        session.projects_root = projects.path().to_path_buf();
        session.session_id = unique_job_id("still-claimed");
        // Stand in for the late worker: the claim is taken and held.
        let _competing_lock = rebon_session::try_acquire_session_active_lock(
            &session.projects_root,
            &session.cwd,
            &session.session_id,
        )
        .unwrap()
        .expect("the competing claim is acquired");
        wait_for(
            &mut session,
            crate::background::PendingHostedSession::handover(job_id.clone()),
        );
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert_parked_on(&session, &job_id);
        assert!(transcript_mentions(&app, "Type to continue"));
    }

    /// The event loop runs far faster than a process starts. Every probe reads
    /// job state, reconciles a pid and can ping a socket on the UI thread, so
    /// the wait is paced — a poll inside the interval must not touch the store
    /// at all.
    #[test]
    fn the_wait_probes_the_store_at_most_once_per_interval() {
        let (_dir, store, job_id) = store_with_job(
            crate::background::BackgroundJobStatus::Succeeded,
            Some("sess-paced"),
            None,
        );
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut pending = crate::background::PendingHostedSession::dispatch(job_id.clone());
        // Pretend a probe just happened.
        assert!(pending.should_probe_now(HOSTED_PROBE_INTERVAL));
        wait_for(&mut session, pending);
        let mut pending_permission = None;

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert!(
            session.pending_hosted_session.is_some(),
            "a poll inside the probe interval must not read the store"
        );
        assert!(app.rebon_tui.transcript.is_empty());

        // Once the interval has passed, the same terminal job is noticed.
        session
            .pending_hosted_session
            .as_mut()
            .expect("still pending")
            .last_probe_at = Some(std::time::Instant::now() - HOSTED_PROBE_INTERVAL * 2);

        poll_pending_hosted_session_in_store(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store,
        );

        assert!(session.pending_hosted_session.is_none());
        assert!(transcript_mentions(&app, "finished"));
    }

    /// Mirroring leaves this session behind on a fresh one, and a running turn
    /// owns the session it is writing into. Ctrl+N mid-turn would swap it out
    /// from under that turn — the same two-owner shape Shift+Enter and
    /// `/hosted` already refuse.
    #[test]
    fn a_hosted_dispatch_refuses_while_a_local_turn_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::new();
        app.agent_view = Some(crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        ));
        let mut session = super::super::test_support::make_test_tui_session();
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let active = Some(ActivePrompt::new(rx, rebon_types::PromptCancel::new()));

        dispatch_agent_view_prompt_hosted(
            &mut app,
            &mut session,
            &active,
            "look at the parser".into(),
            Vec::new(),
        );

        let status = app
            .agent_view
            .as_ref()
            .and_then(|view| view.status.clone())
            .unwrap_or_default();
        assert!(
            status.contains("while a prompt is running"),
            "unexpected status: {status}"
        );
        assert!(session.pending_hosted_session.is_none());
        assert!(session.attached_background_job_id.is_none());
        assert!(
            store.list_jobs().unwrap().is_empty(),
            "a refused dispatch must not leave a job behind"
        );
    }

    /// Resolution eats the tokens it understands, so `@repo` on its own
    /// leaves nothing to say. Dispatching it anyway spends a worker, a
    /// worktree and a provider call to discover the prompt was empty — which
    /// the hosted dispatch already refuses to do.
    #[test]
    fn a_detached_dispatch_with_nothing_left_to_say_is_refused_before_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir.path().join("other-repo");
        std::fs::create_dir_all(sibling.join(".git")).unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::new();
        app.agent_view = Some(crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        ));
        let mut session = super::super::test_support::make_test_tui_session();
        session.cwd = dir.path().to_string_lossy().to_string();

        dispatch_agent_view_prompt_in_store(
            &mut app,
            &session,
            &store,
            "@other-repo".into(),
            Vec::new(),
        );

        let status = app
            .agent_view
            .as_ref()
            .and_then(|view| view.status.clone())
            .unwrap_or_default();
        assert_eq!(status, "background prompt is empty");
        assert!(
            store.list_jobs().unwrap().is_empty(),
            "an empty dispatch must not create a job"
        );
    }

    /// `@repo` is an explicit directory, and resolving it strips the token
    /// from the prompt. Running here cannot change this session's cwd, so the
    /// alternative to refusing is doing the work in the wrong repository with
    /// the request deleted — the one outcome nobody can see happen.
    #[test]
    fn attaching_here_refuses_a_dispatch_aimed_at_another_repository() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir.path().join("other-repo");
        std::fs::create_dir_all(sibling.join(".git")).unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::new();
        app.agent_view = Some(crate::tui::agent_view::AgentViewState::open(
            &store,
            store.list_jobs().unwrap(),
            &app.task_snapshots(),
        ));
        let mut session = super::super::test_support::make_test_tui_session();
        session.cwd = dir.path().to_string_lossy().to_string();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let handle = runtime.handle().clone();
        let mut active_prompt = None;

        dispatch_agent_view_prompt_and_attach(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            "@other-repo fix the build".into(),
            Vec::new(),
        );

        let status = app
            .agent_view
            .as_ref()
            .and_then(|view| view.status.clone())
            .unwrap_or_default();
        assert!(
            status.contains("different directory") && status.contains("other-repo"),
            "unexpected status: {status}"
        );
        assert!(active_prompt.is_none());
        assert!(session.attached_background_job_id.is_none());
        drop(runtime);
    }

    /// `/new`, `/clear`, a resume and a takeover all replace the session and
    /// clear `attached_background_job_id`. A handover started for the old
    /// session must not complete against the new one — it would turn an
    /// unrelated session into some job's mirror. The stale handover retires
    /// itself instead of every ownership change having to know about it.
    #[test]
    fn a_handover_whose_session_was_replaced_retires_itself_silently() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
            unique_job_id("replaced"),
        ));
        // What every ownership change leaves behind.
        session.attached_background_job_id = None;
        let mut pending_permission = None;

        poll_pending_hosted_session(&mut app, &mut session, &mut pending_permission);

        assert!(session.pending_hosted_session.is_none());
        assert!(
            app.rebon_tui.transcript.is_empty(),
            "a session the user has moved on from must not be told about it"
        );

        // Same when the session was attached to a *different* job meanwhile.
        let other = unique_job_id("other-job");
        session.attached_background_job_id = Some(other);
        session.pending_hosted_session = Some(crate::background::PendingHostedSession::handover(
            unique_job_id("replaced-2"),
        ));

        poll_pending_hosted_session(&mut app, &mut session, &mut pending_permission);

        assert!(session.pending_hosted_session.is_none());
        assert!(app.rebon_tui.transcript.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use super::super::test_support::{insert_local_agent_task, make_test_tui_session};

    fn empty_background_runtime_fields() -> crate::background::BackgroundRuntimeFields {
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

    #[test]
    fn background_runtime_preserves_current_service_tier() {
        let app = AppState::new();
        let session = make_test_tui_session();

        session.model.service_tier.set_fast(true);
        assert_eq!(
            background_runtime_from_session(
                &session,
                session.ui_mode,
                app.effort_level,
                app.permission_mode
            )
            .fast_mode,
            Some(true)
        );

        session.model.service_tier.set_fast(false);
        assert_eq!(
            background_runtime_from_session(
                &session,
                session.ui_mode,
                app.effort_level,
                app.permission_mode
            )
            .fast_mode,
            Some(false)
        );
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "gave up waiting for: {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The cwd's scheduler lock goes with the process that runs the session.
    /// A session built as a mirror holds none; the moment it is this
    /// process's after all it takes one, and giving the session away lets
    /// it go at once — so the worker's scheduler wins the lock instead of
    /// probing behind a terminal that would swallow what fires.
    #[test]
    fn the_cron_owner_lock_follows_the_process_that_runs_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let rebon_dir = rebon_tool::cron::tasks::cron_dir(dir.path());
        std::fs::create_dir_all(&rebon_dir).unwrap();
        let lock_is_free = || {
            matches!(
                rebon_core::cron::try_acquire_scheduler_lock(&rebon_dir),
                Ok(Some(_))
            )
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let mut session = super::super::test_support::make_test_tui_session();
        session.cwd = dir.path().to_string_lossy().into_owned();
        assert!(
            session.engine_half.cron_scheduler.is_none(),
            "a session that does not run turns here starts no scheduler"
        );
        assert!(lock_is_free());

        session.start_cron_scheduler();
        assert!(session.engine_half.cron_scheduler.is_some());
        wait_until("the session's scheduler to take the cwd's lock", || {
            !lock_is_free()
        });
        // Twice is once: a second start does not stack a second scheduler.
        session.start_cron_scheduler();

        session.stop_cron_scheduler();
        assert!(session.engine_half.cron_scheduler.is_none());
        wait_until(
            "the lock to be free for the session's new host",
            lock_is_free,
        );
    }

    #[test]
    fn background_attach_recap_uses_recorded_summary() {
        let recap = background_attach_recap_message(
            "bg-1",
            "fix tests",
            crate::background::BackgroundJobStatus::Succeeded,
            Some("result: fixed flaky checkout test"),
        );

        assert!(recap.contains("bg-1"));
        assert!(recap.contains("fix tests"));
        assert!(recap.contains("succeeded"));
        assert!(recap.contains("fixed flaky checkout test"));
    }

    /// `/stop` stops the worker and nothing else: the session stays on
    /// screen as the job's, with the worker's overlay gone and the rows
    /// kept, and the feedback says a prompt brings a new worker.
    #[test]
    fn stop_attached_background_session_marks_job_stopped_and_parks_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::background::BackgroundStore::new(dir.path());
        let mut app = AppState::default();
        let mut session = make_test_tui_session();
        let mut job = store
            .create_job(
                "stop this job".into(),
                PathBuf::from("."),
                empty_background_runtime_fields(),
            )
            .unwrap();
        job.process.status = crate::background::BackgroundJobStatus::Idle;
        job.identity.session_id = Some(session.session_id.clone());
        store.write_state(&job).unwrap();
        session.attached_background_job_id = Some(job.job_id().to_string());
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                job.job_id().to_string(),
                session.session_id.clone(),
                session.cwd.clone(),
                job.status(),
                job.event_count(),
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "remote-stop".into(),
                },
            ));
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("remote partial".into()),
        );
        app.plan_entries.push(rebon_types::PlanEntry {
            content: "remote plan".into(),
            priority: rebon_types::PlanEntryPriority::High,
            status: rebon_types::PlanEntryStatus::InProgress,
        });

        let stopped =
            stop_attached_background_session_in_store(&mut app, &mut session, &store).unwrap();

        assert!(
            stopped.feedback.contains(&job.job_id()),
            "the sentence says which worker was stopped: {}",
            stopped.feedback
        );
        assert!(
            stopped.feedback.contains("Type to continue"),
            "{}",
            stopped.feedback
        );
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some(job.job_id())
        );
        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("the session stays parked on the job");
        assert!(!remote.is_live());
        assert_eq!(
            remote.status,
            crate::background::BackgroundJobStatus::Stopped
        );
        assert!(app.rebon_tui.overlay.is_empty());
        assert!(app.plan_entries.is_empty());
        assert!(!app.is_loading);
        let state = store.read_state(&job.job_id()).unwrap();
        assert_eq!(
            state.status(),
            crate::background::BackgroundJobStatus::Stopped
        );
        assert!(
            app.agent_view.is_none(),
            "the session stays on screen; the list is a Ctrl+Z away"
        );
    }

    #[test]
    fn agent_view_stop_and_remove_task_releases_stuck_agent() {
        let mut app = AppState::new();
        let registry = rebon_plugin_tasks::runtime::TaskRegistry::new();
        let cancel = insert_local_agent_task(
            &registry,
            "agent-stuck",
            rebon_plugin_tasks::runtime::TaskStatus::Running,
            true,
        );
        app.tasks = Arc::new(registry);
        let mut session = make_test_tui_session();
        session.engine_half.tasks = app.tasks.clone();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let handle = runtime.handle().clone();
        let mut active_prompt = None;

        assert!(!handle_agent_view_outcome(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            AgentViewKeyOutcome::CancelOrExit,
        ));

        assert!(handle_agent_view_outcome(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            AgentViewKeyOutcome::StopTask {
                task_id: "agent-stuck".into(),
            },
        ));
        assert!(cancel.is_cancelled());
        assert_eq!(
            app.tasks
                .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-stuck"))
                .expect("stopped task")
                .status,
            rebon_plugin_tasks::runtime::TaskStatus::Killed
        );

        assert!(handle_agent_view_outcome(
            &mut app,
            &mut session,
            &handle,
            &mut active_prompt,
            AgentViewKeyOutcome::RemoveTask {
                task_id: "agent-stuck".into(),
            },
        ));
        assert!(app
            .tasks
            .snapshot(&rebon_plugin_tasks::runtime::TaskId::new("agent-stuck"))
            .is_none());
    }
}
