use std::collections::HashSet;
use std::time::{Duration, Instant};

use rebon_core::permission::{OutboundPermissionQuery, PermissionQueryOption};
use tokio::sync::oneshot::error::TryRecvError;

use crate::background::{
    clear_remote_turn_projection, cover_settled_turn_entries, drain_owner_words,
    last_remote_user_turn_uuid, merge_local_system_rows, permission_option_kind,
    persisted_remote_turn, projected_tool_ids, projection_covers_persisted_turn,
    prune_stale_coverage, remote_turn_is_absorbed, sync_turn_with_record, transcript_fingerprint,
    update_targets_settled_tool, MergeStreamingContext, OwnerWord, PersistedRemoteTurn,
};
use crate::session::transcript_replay::BackgroundAgentTaskRef;
use crate::tui::app::{AppState, PromptCompletionStatus};
use crate::tui::permission_modal::PendingPermission;
use crate::tui::wiring::TuiEngineSession;

use super::permission_flow::build_pending_permission;
use super::transcript_messages::inject_system_message;
use super::transcript_replay::replay_transcript_entries_with_agent_tasks;

const REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const TRANSCRIPT_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

pub(super) fn refresh_remote_background_attachment(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
) {
    let now = Instant::now();
    let Some(remote) = session.remote_background_attachment.as_ref() else {
        return;
    };
    // The owner's deltas run every frame; only the rest waits for the interval.
    //
    // Draining them is a channel read and a projection — no disk, no endpoint
    // probe — while the remainder of this refresh reads the job record off disk
    // and pings the worker, which is what the interval is for. Throttling both
    // together meant a token the owner had already sent, and this process was
    // already holding, waited up to `REFRESH_INTERVAL` to reach the screen.
    // That wait was the whole of the difference a mirrored session had from a
    // local one mid-stream: a local session paints a chunk when it arrives, and
    // now so does this.
    //
    // Safe to run early because applying is self-guarded:
    // `apply_pending_stream_updates` holds its deltas back while the mirror is
    // still initialising or catching up an announced gap, so nothing can land
    // ahead of the file read it is meant to follow.
    let interval_due = now.duration_since(remote.last_refresh_at) >= REFRESH_INTERVAL;
    super::live_agent_view::with_main_agent_view(app, |app| {
        drain_owner_events(app, session, pending_permission);
        apply_pending_stream_updates(app, session);
        if interval_due {
            refresh_remote_background_attachment_main(app, session, pending_permission, now);
        }
        // What the event loop reads as "a turn is running" for a mirror, kept
        // in step with `app.is_loading` for whatever renders before the loop
        // next derives it.
        if let Some(remote) = session.remote_background_attachment.as_ref() {
            app.is_loading = remote.running_turn_started_at().is_some();
        }
    });
}

fn refresh_remote_background_attachment_main(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    now: Instant,
) {
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return;
    };
    remote.last_refresh_at = now;

    // A replacement worker is on its way and the handover poll is watching
    // for it. The attachment stays in place meanwhile — dropping it would
    // flush every row it was holding into the scrollback and repaint the
    // session when the new worker is mirrored — but there is nothing at its
    // old endpoint to refresh from, and probing it again would only queue
    // another worker.
    if session
        .pending_hosted_session
        .as_ref()
        .is_some_and(|pending| {
            matches!(
                pending.kind,
                crate::background::PendingHostedKind::Reattach { .. }
            )
        })
    {
        return;
    }

    let Some(remote) = session.remote_background_attachment.as_ref() else {
        return;
    };
    let job_id = remote.job_id.clone();
    let should_refresh_transcript_interval =
        now.duration_since(remote.last_transcript_refresh_at) >= TRANSCRIPT_REFRESH_INTERVAL;
    let store = crate::background::cli_default_store();
    let state = match crate::background::mirrored_job_state_in_store(&store, &job_id) {
        Ok(state) => state,
        Err(err) => {
            // The job record itself is unreadable, so there is nothing to
            // bring a worker back from. The session stays on screen as the
            // job's — it does not come back here, it never does — and the
            // next prompt, or a record that is readable again, is what gives
            // it a worker. Said once: a record that stays unreadable would
            // otherwise say so ten times a second.
            let Some(remote) = session.remote_background_attachment.as_mut() else {
                return;
            };
            if remote.is_live() {
                remote.worker_gone(remote.status);
                *pending_permission = None;
                app.rebon_tui.overlay.clear();
                app.is_loading = false;
                app.remote_background_tasks.clear();
                inject_system_message(
                    app,
                    "error",
                    &format!(
                        "Lost worker {job_id}: {err}. Type to start another for this session, or Ctrl+Z for the agent list."
                    ),
                );
                app.follow_transcript_tail = true;
            } else {
                tracing::debug!(%job_id, %err, "parked session: job record still unreadable");
            }
            return;
        }
    };

    if let Some(endpoint) = remote.endpoint() {
        let state_endpoint_matches = state.pid() == Some(endpoint.pid)
            && state.ipc_port() == Some(endpoint.port)
            && state.ipc_token() == Some(endpoint.token.as_str());
        if !state_endpoint_matches {
            if recover_lost_worker(app, session, pending_permission, &store) {
                return;
            }
        } else if !remote_endpoint_is_healthy(session, now)
            && recover_lost_worker(app, session, pending_permission, &store)
        {
            return;
        }

        // Whatever the owner has pushed since the last frame. Cheap and
        // non-blocking, and it is how this terminal learns facts the job
        // record does not carry — session token spend above all.
        drain_owner_events(app, session, pending_permission);

        sync_remote_permission(
            app,
            session,
            pending_permission,
            state.outcome.pending_permission.clone(),
        );
        relay_permission_answer(app, session);
        relay_forwarded_command_output(app, session);
        sync_permission_mode_with_worker(app, session, state.runtime().permission_mode.as_deref());
    } else {
        // Nothing to follow. The record is still read: somebody else may
        // have given the job a worker (`rebon attach` from another
        // terminal), and a worker that was late rather than dead may have
        // turned up. Either is followed; neither is started from here.
        follow_worker_given_elsewhere(app, session, &store, now);
    }

    // Whether the owner's stream is the word on this session's turns.
    //
    // An owner that has said hello with a numbered stream announces every turn
    // it starts and ends, before the first token and after the last, and the
    // job record's copy of the same fact lags each of those by a write. Read
    // both and the later one wins for a tick: the record still says `Running`
    // after the stream said the turn ended, and the spinner comes back for a
    // frame after it went out. So while the stream speaks, the record decides
    // nothing about the turn — with one exception below.
    let streaming_turns = session
        .remote_background_attachment
        .as_ref()
        .and_then(|remote| remote.worker.as_ref())
        .is_some_and(|worker| worker.stream_delivers_deltas());
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.status = state.status();
        remote.owner_retry = state.retry();
        if !remote.turn_is_terminal() {
            remote.terminal_transcript_synced = false;
        }
        sync_turn_with_record(remote, &state, streaming_turns, now);
    }
    let event_count_changed = session
        .remote_background_attachment
        .as_ref()
        .is_some_and(|remote| remote.last_event_count != state.event_count());
    pump_remote_session_updates(app, session, &store, state.event_count());

    if !streaming_turns {
        app.prompt_completion_status = crate::background::mirrored_job_completion(state.status())
            .map(|completion| match completion {
                crate::background::MirroredJobCompletion::Succeeded => {
                    PromptCompletionStatus::Succeeded
                }
                crate::background::MirroredJobCompletion::Failed => PromptCompletionStatus::Failed,
            });
    }
    app.is_loading = session
        .remote_background_attachment
        .as_ref()
        .is_some_and(|remote| remote.running_turn_started_at().is_some());
    let terminal_turn = crate::background::mirrored_job_is_terminal(state.status());

    let (transcript_never_loaded, awaiting_overlay_absorption, terminal_transcript_pending) =
        session
            .remote_background_attachment
            .as_ref()
            .map(|remote| {
                (
                    remote.transcript_fingerprint == 0,
                    remote.awaiting_overlay_absorption,
                    remote.turn_is_terminal() && !remote.terminal_transcript_synced,
                )
            })
            .unwrap_or((false, false, false));
    // Whether the owner's deltas are actually reaching this mirror.
    //
    // While they are, the persisted transcript is behind *by construction* —
    // it is written per message, the stream per chunk — so rebuilding from it
    // mid-turn swaps the in-flight text for a copy that does not contain it
    // yet, and the next delta puts it back. That is the flicker. The events
    // file read next door already carries exactly this guard
    // (`file_changed && !streaming`); the transcript rebuild never got it.
    let streaming = session
        .remote_background_attachment
        .as_ref()
        .and_then(|remote| remote.worker.as_ref())
        .is_some_and(|worker| worker.stream_delivers_deltas() && worker.events_rx.is_some());
    // Transcript refresh is otherwise independent from the live-event gate. It
    // remains active during the first load, or while a projected turn is
    // waiting for its persisted boundary/content to catch up. A terminal
    // transition bypasses the interval so its final rows are loaded before the
    // mutable inline tail is released into immutable scrollback — which is why
    // gating the mid-turn rebuild does not cost the end-of-turn one.
    let should_refresh_transcript = terminal_transcript_pending
        || (should_refresh_transcript_interval
            && (event_count_changed
                || (app.is_loading && !streaming)
                || awaiting_overlay_absorption
                || transcript_never_loaded));

    if should_refresh_transcript {
        let refreshed = refresh_remote_transcript(app, session, terminal_turn);
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            remote.last_transcript_refresh_at = now;
            if terminal_turn && refreshed {
                remote.terminal_transcript_synced = true;
            }
        }
    }
    if event_count_changed || should_refresh_transcript {
        sync_remote_tasks(app, &store, &job_id);
    }
}

fn remote_endpoint_is_healthy(session: &mut TuiEngineSession, now: Instant) -> bool {
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return false;
    };
    let job_id = remote.job_id.clone();
    let Some(worker) = remote.worker.as_mut() else {
        return false;
    };
    worker.endpoint_is_healthy(&job_id, now)
}

/// How often a parked attachment reads the job record for a worker.
const PARKED_WORKER_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// What a parked session says when it is given up on, or stopped.
pub(super) fn parked_session_hint() -> &'static str {
    "Type to continue it in a new worker, or Ctrl+Z for the agent list."
}

/// Let go of the worker, keep the session.
///
/// Everything on screen stays; the streaming overlay, which was the
/// worker's, does not. What the record says the job is now is what the
/// attachment says.
pub(super) fn park_attachment(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    status: crate::background::BackgroundJobStatus,
) {
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return;
    };
    if pending_permission
        .as_ref()
        .is_some_and(|pending| remote.pending_permission_query_id == Some(pending.outbound.id))
    {
        *pending_permission = None;
    }
    remote.worker_gone(status);
    app.rebon_tui.overlay.clear();
    app.is_loading = false;
    app.remote_background_tasks.clear();
}

/// A parked attachment follows a worker the job was given elsewhere.
///
/// Not a revival — nothing is queued from here, which is what keeps a
/// `rebon stop` from being undone by whoever was watching. But a worker
/// that exists is the session's host, wherever it came from: another
/// terminal's `rebon attach`, or the one this terminal gave up waiting for.
/// The record is enough to link; the health probe takes it from there.
fn follow_worker_given_elsewhere(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    store: &crate::background::BackgroundStore,
    now: Instant,
) {
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return;
    };
    if now.duration_since(remote.last_worker_probe_at) < PARKED_WORKER_PROBE_INTERVAL {
        return;
    }
    remote.last_worker_probe_at = now;
    let job_id = remote.job_id.clone();
    let Some(endpoint) = crate::background::worker_given_elsewhere_in_store(store, &job_id) else {
        return;
    };
    remote.link_worker(endpoint);
    super::inject_local_command_feedback(app, "attach", &format!("Attached to worker {job_id}."));
    app.follow_transcript_tail = true;
}

/// The worker this mirror was watching is not answering. Either the job
/// already has a new one (follow it), or it has none (queue one and wait for
/// it, keeping the view). The session never comes back to this process.
///
/// Returns whether the caller should stop this refresh pass.
fn recover_lost_worker(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    store: &crate::background::BackgroundStore,
) -> bool {
    let Some(job_id) = session
        .remote_background_attachment
        .as_ref()
        .map(|remote| remote.job_id.clone())
    else {
        return true;
    };
    match crate::background::recover_lost_worker_in_store(store, &job_id) {
        crate::background::LostWorkerRecovery::Stopped { status } => {
            // A deliberate stop is terminal. The owner checked it before any
            // attach/revival attempt, so watching can never resurrect it.
            park_attachment(app, session, pending_permission, status);
            super::inject_local_command_feedback(
                app,
                "attach",
                &format!("Worker {job_id} was stopped. {}", parked_session_hint()),
            );
            app.follow_transcript_tail = true;
            true
        }
        crate::background::LostWorkerRecovery::Follow(endpoint) => {
            if let Some(remote) = session.remote_background_attachment.as_mut() {
                if remote.endpoint().as_ref() != Some(&endpoint) {
                    if pending_permission.as_ref().is_some_and(|pending| {
                        remote.pending_permission_query_id == Some(pending.outbound.id)
                    }) {
                        *pending_permission = None;
                    }
                    remote.pending_permission_query_id = None;
                    remote.pending_permission_turn_generation = None;
                    remote.pending_permission_endpoint = None;
                    remote.pending_permission_rx = None;
                    // A fresh link drops the old stream and its in-flight
                    // endpoint probe, so stale failure cannot trigger fallback.
                    remote.link_worker(endpoint);
                }
                if let Some(worker) = remote.worker.as_mut() {
                    worker.last_endpoint_check_at = Instant::now();
                }
            }
            false
        }
        crate::background::LostWorkerRecovery::Starting {
            job_id: replacement_job_id,
        } => {
            // Keep the attachment and view in place while the same session is
            // remirrored. No local session or second state is created.
            let status = session
                .remote_background_attachment
                .as_ref()
                .expect("lost-worker recovery requires an attachment")
                .status;
            park_attachment(app, session, pending_permission, status);
            session.pending_hosted_session = Some(
                crate::background::PendingHostedSession::reattach(replacement_job_id, true),
            );
            super::inject_local_command_feedback(
                app,
                "attach",
                &format!("Worker {job_id} went away — starting another one for this session…"),
            );
            app.follow_transcript_tail = true;
            true
        }
        crate::background::LostWorkerRecovery::OwnerStillRecorded { error } => {
            if let Some(worker) = session
                .remote_background_attachment
                .as_mut()
                .and_then(|remote| remote.worker.as_mut())
            {
                worker.endpoint_probe_rx = None;
                worker.last_endpoint_check_at = Instant::now();
            }
            tracing::warn!(
                %error,
                %job_id,
                "background worker IPC is unavailable but its owner is still recorded; keeping the mirror"
            );
            false
        }
        crate::background::LostWorkerRecovery::Failed { status, error } => {
            park_attachment(app, session, pending_permission, status);
            inject_system_message(
                app,
                "error",
                &format!(
                    "Lost worker {job_id} and could not start another: {error}. {}",
                    parked_session_hint()
                ),
            );
            app.follow_transcript_tail = true;
            true
        }
    }
}

fn relay_permission_answer(app: &mut AppState, session: &mut TuiEngineSession) {
    let pending_answer = {
        let Some(remote) = session.remote_background_attachment.as_mut() else {
            return;
        };
        let Some(receiver) = remote.pending_permission_rx.as_mut() else {
            return;
        };
        match receiver.try_recv() {
            Ok(answer) => {
                remote.pending_permission_rx = None;
                let Some(query_id) = remote.pending_permission_query_id.take() else {
                    return;
                };
                let Some(turn_generation) = remote.pending_permission_turn_generation.take() else {
                    return;
                };
                let Some(endpoint) = remote.pending_permission_endpoint.take() else {
                    return;
                };
                Some((
                    remote.job_id.clone(),
                    query_id,
                    turn_generation,
                    endpoint,
                    answer,
                ))
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Closed) => {
                remote.pending_permission_rx = None;
                remote.pending_permission_query_id = None;
                remote.pending_permission_turn_generation = None;
                remote.pending_permission_endpoint = None;
                None
            }
        }
    };
    let Some((job_id, query_id, turn_generation, endpoint, answer)) = pending_answer else {
        return;
    };
    if let Err(err) = crate::background::answer_remote_permission(
        &job_id,
        query_id,
        turn_generation,
        &endpoint,
        answer,
    ) {
        inject_system_message(
            app,
            "error",
            &format!("Failed to answer background permission: {err}"),
        );
    }
}

/// Keep this UI's permission mode and the worker's the same, in both
/// directions.
///
/// Outbound: Shift+Tab, the settings dialog and `/mode` all write this
/// process's mirror cell, while the broker that actually asks lives in the
/// worker, so a local change has to be pushed.
///
/// Inbound: more than one UI can mirror the same worker, and the one that
/// did not make the change has no other way to learn about it. The worker
/// publishes the mode in force to the job state; a value that differs from
/// what this UI shows — and that this UI is not itself mid-push toward —
/// wins, because the worker is where the mode is actually enforced. A UI
/// displaying a restriction the worker is not applying is the failure this
/// whole path exists to avoid.
fn sync_permission_mode_with_worker(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    published: Option<&str>,
) {
    let current = app.permission_mode;
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return;
    };
    if let Some(receiver) = remote.mode_sync_rx.as_ref() {
        match receiver.try_recv() {
            Ok(Ok(())) => remote.mode_sync_rx = None,
            Ok(Err(err)) => {
                remote.mode_sync_rx = None;
                // Re-arm: the mode the user sees was never applied, so the
                // next refresh tries again rather than leaving them wrong.
                remote.synced_permission_mode = None;
                inject_system_message(
                    app,
                    "error",
                    &format!("Permission mode was not applied to the background session: {err}"),
                );
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => remote.mode_sync_rx = None,
        }
        return;
    }
    let Some(endpoint) = remote.endpoint() else {
        return;
    };
    match remote.synced_permission_mode {
        None => remote.synced_permission_mode = Some(current),
        Some(synced) if synced != current => {
            let receiver =
                crate::background::spawn_remote_permission_mode(&remote.job_id, &endpoint, current);
            remote.synced_permission_mode = Some(current);
            remote.mode_sync_rx = Some(receiver);
            return;
        }
        Some(_) => {}
    }

    // Nothing of ours is in flight, so a published mode that disagrees with
    // the display came from somewhere else — another UI on this same worker,
    // or a respawn. Adopt it: the worker is what enforces.
    let Some(worker_mode) = published.map(rebon_permissions::PermissionMode::from_wire) else {
        return;
    };
    if worker_mode == current {
        return;
    }
    remote.synced_permission_mode = Some(worker_mode);
    app.set_permission_mode(worker_mode);
}

/// Surface a forwarded session-control command's answer once it lands.
///
/// The command ran in the worker against the real session; its output is
/// the user's receipt, so a failure — including a worker that died holding
/// the request — has to say so by name rather than leaving the prompt to
/// look ignored.
fn relay_forwarded_command_output(app: &mut AppState, session: &mut TuiEngineSession) {
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return;
    };
    let Some(pending) = remote.pending_command.as_ref() else {
        return;
    };
    let outcome = match pending.rx.try_recv() {
        Ok(result) => result,
        Err(std::sync::mpsc::TryRecvError::Empty) => return,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            Err("the background worker dropped the request without answering".to_string())
        }
    };
    let name = pending.name.clone();
    remote.pending_command = None;

    match outcome {
        Ok(output) => {
            let level = if output.tone == "warning" {
                "warning"
            } else {
                "info"
            };
            inject_system_message(app, level, &output.text);
        }
        Err(err) => inject_system_message(
            app,
            "error",
            &format!("/{name} did not run in the attached background session: {err}"),
        ),
    }
    app.follow_transcript_tail = true;
}

fn sync_remote_permission(
    app: &AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    snapshot: Option<crate::background::BackgroundPermissionQuerySnapshot>,
) {
    let Some((current_query_id, current_turn_generation, current_endpoint, attached_endpoint)) =
        session
            .remote_background_attachment
            .as_ref()
            .and_then(|remote| {
                Some((
                    remote.pending_permission_query_id,
                    remote.pending_permission_turn_generation,
                    remote.pending_permission_endpoint.clone(),
                    remote.endpoint()?,
                ))
            })
    else {
        return;
    };
    let Some(snapshot) = snapshot else {
        if let Some(query_id) = current_query_id {
            if pending_permission
                .as_ref()
                .is_some_and(|pending| pending.outbound.id == query_id)
            {
                *pending_permission = None;
            }
            if let Some(remote) = session.remote_background_attachment.as_mut() {
                remote.pending_permission_query_id = None;
                remote.pending_permission_turn_generation = None;
                remote.pending_permission_endpoint = None;
                remote.pending_permission_rx = None;
            }
        }
        return;
    };
    let Some(snapshot_endpoint) = snapshot.endpoint.clone() else {
        return;
    };
    if snapshot_endpoint != attached_endpoint {
        if pending_permission
            .as_ref()
            .is_some_and(|pending| Some(pending.outbound.id) == current_query_id)
        {
            *pending_permission = None;
        }
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            remote.pending_permission_query_id = None;
            remote.pending_permission_turn_generation = None;
            remote.pending_permission_endpoint = None;
            remote.pending_permission_rx = None;
        }
        return;
    }
    if current_query_id == Some(snapshot.query_id)
        && current_turn_generation == Some(snapshot.turn_generation)
        && current_endpoint.as_ref() == Some(&snapshot_endpoint)
    {
        return;
    }
    if pending_permission.is_some() {
        let replaces_remote_query = current_query_id.is_some()
            && pending_permission
                .as_ref()
                .is_some_and(|pending| Some(pending.outbound.id) == current_query_id);
        if !replaces_remote_query {
            return;
        }
        *pending_permission = None;
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            remote.pending_permission_query_id = None;
            remote.pending_permission_turn_generation = None;
            remote.pending_permission_endpoint = None;
            remote.pending_permission_rx = None;
        }
    }

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let outbound = OutboundPermissionQuery {
        id: snapshot.query_id,
        tool_name: snapshot.tool.clone().unwrap_or_default(),
        tool_call_id: snapshot.tool_call_id.clone().unwrap_or_default(),
        session_id: snapshot
            .session_id
            .clone()
            .unwrap_or_else(|| session.session_id.clone()),
        title: snapshot.title.clone().unwrap_or_else(|| {
            format!("{} permission", snapshot.tool.as_deref().unwrap_or("Tool"))
        }),
        message: snapshot.message.clone().unwrap_or_else(|| {
            format!(
                "Allow {}?",
                snapshot.tool.as_deref().unwrap_or("background tool")
            )
        }),
        tool_input: snapshot.tool_input.clone(),
        metadata: snapshot.metadata.clone(),
        options: snapshot
            .options
            .iter()
            .map(|option| PermissionQueryOption {
                option_id: option.option_id.clone(),
                label: option.label.clone(),
                kind: permission_option_kind(&option.kind),
            })
            .collect(),
        response_tx,
    };
    *pending_permission = Some(build_pending_permission(app, outbound));
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.pending_permission_query_id = Some(snapshot.query_id);
        remote.pending_permission_turn_generation = Some(snapshot.turn_generation);
        remote.pending_permission_endpoint = Some(snapshot_endpoint);
        remote.pending_permission_rx = Some(response_rx);
    }
}

/// Fold freshly appended `session_update` events from the attached worker into
/// the read-only projector. Reads are gated by the durable event count; the
/// initial scan locates the final queued-user boundary and only rebuilds that
/// turn's streaming suffix.
/// Apply what the owner pushed since the last frame.
///
/// Only the facts that are safe to apply repeatedly: a status snapshot is the
/// owner's whole answer to "what should every client agree about", so applying
/// the same one twice changes nothing. Streaming deltas are deliberately not
/// folded here — those still come from the event log, which is offset-based
/// and therefore cannot double-apply. Mixing two unsynchronised sources for
/// the same deltas would.
/// Take in whatever the owner has streamed since the last frame.
///
/// Snapshots (`hello`, `status`) are applied here. Deltas are not: they are
/// held on the link for [`pump_remote_session_updates`], which applies them
/// after any read of the event log the frame owes — so a delta the file is
/// about to say lands once, in the owner's order. Turn transitions and
/// permission prompts reach this terminal by their existing routes.
fn drain_owner_events(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
) {
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return;
    };
    let words = drain_owner_words(remote);
    for word in words {
        match word {
            OwnerWord::Hello(snapshot) => {
                let presentation = {
                    let Some(remote) = session.remote_background_attachment.as_mut() else {
                        continue;
                    };
                    crate::background::apply_owner_hello(remote, *snapshot, Instant::now())
                };
                apply_owner_status(app, session, pending_permission, presentation);
            }
            OwnerWord::Status(snapshot) => {
                let presentation = {
                    let Some(remote) = session.remote_background_attachment.as_mut() else {
                        continue;
                    };
                    crate::background::apply_owner_status(remote, *snapshot)
                };
                apply_owner_status(app, session, pending_permission, presentation);
            }
            OwnerWord::Turn { state, stop_reason } => {
                let outcome = {
                    let Some(remote) = session.remote_background_attachment.as_mut() else {
                        continue;
                    };
                    crate::background::apply_owner_turn(remote, state, stop_reason, Instant::now())
                };
                app.prompt_completion_status = match outcome {
                    crate::background::OwnerTurnOutcome::Running => None,
                    crate::background::OwnerTurnOutcome::Finished { succeeded: true } => {
                        Some(PromptCompletionStatus::Succeeded)
                    }
                    crate::background::OwnerTurnOutcome::Finished { succeeded: false } => {
                        Some(PromptCompletionStatus::Failed)
                    }
                };
            }
            OwnerWord::Permission(query) => {
                sync_remote_permission(app, session, pending_permission, Some(*query));
            }
        }
    }
}

/// Apply the owner-owned snapshot's narrow presentation values to this TUI.
fn apply_owner_status(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    presentation: crate::background::OwnerStatusPresentation,
) {
    let crate::background::OwnerStatusPresentation {
        usage,
        pending_permission: owner_permission,
        permission_mode,
        effort,
    } = presentation;
    if let Some(usage) = usage {
        let mut ledger = app.usage_mut();
        let mut total = ledger.snapshot().total;
        total.input_tokens = usage.input_tokens;
        total.output_tokens = usage.output_tokens;
        total.cache_read_input_tokens = usage.cache_read_input_tokens;
        total.cache_creation_input_tokens = usage.cache_creation_input_tokens;
        ledger.adopt_owner_total(total);
    }
    sync_remote_permission(app, session, pending_permission, owner_permission);
    sync_permission_mode_with_worker(app, session, permission_mode.as_deref());
    match effort.as_deref() {
        None => app.effort_level = None,
        Some(effort) => {
            if let Some(level) = crate::session::commands::effort::effort_level_from_wire(effort) {
                app.effort_level = Some(level);
            }
        }
    }
}

/// Fold the owner's session updates into the screen.
///
/// The stream is the source while the owner speaks one: a token the owner
/// has is on screen the frame it arrives, not a file read and a tick later.
/// The event log is read for what the stream cannot give — the first scan
/// that rebuilds the turn in flight, a gap the owner announced, a stream that
/// ended before it could be reopened, or an owner that does not stream — and
/// lines the stream already delivered are recognized by the cursor the owner
/// stamped them with, so the two sources never render the same delta twice.
fn pump_remote_session_updates(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    store: &crate::background::BackgroundStore,
    event_count: u64,
) -> bool {
    let Some(remote) = session.remote_background_attachment.as_ref() else {
        return false;
    };
    let initializing = !remote.live_events_initialized;
    let file_changed = remote.last_event_count != event_count;
    let (streaming, catching_up) = remote
        .worker
        .as_ref()
        .map(|worker| {
            (
                worker.stream_delivers_deltas() && worker.events_rx.is_some(),
                worker.catching_up(),
            )
        })
        .unwrap_or((false, false));
    let read_file = initializing || catching_up || (file_changed && !streaming);
    if read_file {
        tracing::debug!(
            job_id = %remote.job_id,
            initializing,
            catching_up,
            streaming,
            file_changed,
            "mirror: reading the owner's events file"
        );
    }
    let mut applied = false;
    if read_file {
        applied = read_remote_session_updates_from_file(app, session, store, event_count);
    } else if file_changed {
        // The stream has these, or will. The count is taken so the next frame
        // does not ask again; the offset is not, so a stream lost later is
        // filled in from the last line the file was actually read to.
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            remote.last_event_count = event_count;
        }
    }
    applied |= apply_pending_stream_updates(app, session);
    applied
}

/// Apply the deltas the stream delivered, now that the file has had its say.
fn apply_pending_stream_updates(app: &mut AppState, session: &mut TuiEngineSession) -> bool {
    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return false;
    };
    if !remote.live_events_initialized {
        return false;
    }
    let session_id = remote.session_id.clone();
    let Some(worker) = remote.worker.as_mut() else {
        return false;
    };
    if worker.catching_up() || worker.pending_stream_updates.is_empty() {
        return false;
    }
    let pending = std::mem::take(&mut worker.pending_stream_updates);
    let mut applied = false;
    let mut delta_count = 0usize;
    for (cursor, params) in pending {
        let Some(worker) = session
            .remote_background_attachment
            .as_mut()
            .and_then(|remote| remote.worker.as_mut())
        else {
            break;
        };
        if !worker.mark.accepts_update(cursor) {
            continue;
        }
        worker.mark.note_update_applied(cursor);
        if params.session_id != session_id {
            continue;
        }
        project_remote_update(app, session, params, false);
        applied = true;
        delta_count += 1;
    }
    if delta_count > 0 {
        tracing::debug!(
            %session_id,
            deltas = delta_count,
            "mirror: applied deltas from the owner's stream"
        );
    }
    applied
}

fn read_remote_session_updates_from_file(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    store: &crate::background::BackgroundStore,
    event_count: u64,
) -> bool {
    let read = {
        let Some(remote) = session.remote_background_attachment.as_mut() else {
            return false;
        };
        crate::background::read_remote_session_updates_from_store(remote, store)
    };
    let Some((updates, new_offset, initializing)) = read else {
        return false;
    };

    if initializing {
        let current_turn_start = updates.iter().rposition(|params| {
            matches!(
                &params.update,
                rebon_types::SessionUpdate::QueuedUserMessage { .. }
            )
        });
        let snapshot_end = current_turn_start.unwrap_or(updates.len());
        for params in &updates[..snapshot_end] {
            crate::tui::update::project_remote_session_snapshot_update(app, &params.update);
        }
        if let Some(start) = current_turn_start {
            for params in updates.into_iter().skip(start) {
                project_remote_update(app, session, params, true);
            }
        }
    } else {
        for params in updates {
            project_remote_update(app, session, params, false);
        }
    }

    if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.live_events_offset = new_offset;
        remote.last_event_count = event_count;
        remote.live_events_initialized = true;
        if initializing && remote.awaiting_overlay_absorption {
            remote.initial_overlay_fingerprint = Some(remote.transcript_fingerprint);
        }
    }
    true
}

fn begin_remote_turn(
    app: &mut AppState,
    remote: &mut crate::background::RemoteBackgroundAttachment,
    user_uuid: String,
    historical_rebuild: bool,
) {
    // A fast-follow prompt's `QueuedUserMessage` arrives on the stream
    // ahead of the file's word on the turn it ends. Wiping the projection
    // first dropped that turn to the splice and printed its tail a second
    // time under the new prompt — so a watched turn settles from its own
    // stream right here. Replayed boundaries during an attach rebuild are
    // exempt: the primed store already holds those turns' rows, and
    // committing their replayed overlays would duplicate them.
    if !historical_rebuild {
        settle_turn_in_flight_on_handoff(app, remote);
    }
    // What still sits in the overlay after the handoff depends on where
    // this boundary came from. A previous turn was tracked: leftovers are
    // its splice-world remains (or replayed history) and must not bleed
    // into the new turn — clear. No previous turn was ever tracked: the
    // only thing that can be here is the NEW turn's own early deltas —
    // the file boundary can land in the same refresh frame as a fast
    // first token — and clearing would eat the head of the reply.
    let first_boundary_ever = !historical_rebuild && remote.current_turn_user_uuid.is_none();
    if !first_boundary_ever {
        app.rebon_tui.overlay.clear();
        remote.remote_hidden_tool_call_ids.clear();
        clear_remote_turn_projection(remote);
    }
    remote.current_turn_user_uuid = Some(user_uuid);
    remote.current_turn_watched = true;
}

/// Adopt the file's newest user turn as the boundary.
///
/// The stream never announces a boundary for an immediate prompt —
/// `queued_user_message` fires only for queued/steered prompts and
/// AskUserQuestion answers, which ordinary turns never emit — so every
/// turn used to run on a boundary set once at attach, or never on a
/// fresh session: no
/// absorption, no withholding, no settle, and the whole watched-turn
/// machinery sat inert while the splice and the overlay each printed a
/// copy. The file's user entry is written at the prompt's admission,
/// ahead of the first delta in all but pathological TTFTs, and this sync
/// picks it up within a transcript refresh tick. The begin path settles a
/// watched previous turn before it clears anything.
fn sync_turn_boundary_from_file(
    app: &mut AppState,
    remote: &mut crate::background::RemoteBackgroundAttachment,
    boundary: Option<String>,
) {
    let Some(boundary) = boundary else {
        return;
    };
    if remote.current_turn_user_uuid.as_deref() == Some(boundary.as_str()) {
        return;
    }
    tracing::info!(
        target: "stream_dbg",
        %boundary,
        previous = remote.current_turn_user_uuid.as_deref().unwrap_or("<none>"),
        overlay_blocks = app.rebon_tui.overlay.blocks.len(),
        "mirror: adopting turn boundary from the file"
    );
    begin_remote_turn(app, remote, boundary, false);
}

/// Settle the turn in flight when the next one begins, from the stream
/// alone. Only for a turn still provably watched live — one the splice
/// already touched keeps the splice. Its persisted entries are covered
/// later, against the kept snapshot, as they reach the file.
fn settle_turn_in_flight_on_handoff(
    app: &mut AppState,
    remote: &mut crate::background::RemoteBackgroundAttachment,
) {
    if !remote.awaiting_overlay_absorption || !remote.current_turn_watched {
        return;
    }
    let Some(user_uuid) = remote.current_turn_user_uuid.clone() else {
        return;
    };
    let watched_something = !remote.current_turn_projected_text.is_empty()
        || !remote.current_turn_projected_thinking.is_empty()
        || !remote.current_turn_visible_tool_call_ids.is_empty()
        || !remote.remote_hidden_tool_call_ids.is_empty();
    if !watched_something {
        return;
    }
    tracing::info!(
        target: "stream_dbg",
        %user_uuid,
        "mirror: settled watched turn at handoff to the next prompt"
    );
    commit_watched_turn_locally(app, remote, user_uuid);
}

fn project_remote_update(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    params: rebon_types::SessionUpdateParams,
    historical_rebuild: bool,
) {
    if let rebon_types::SessionUpdate::QueuedUserMessage { uuid, .. } = &params.update {
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            begin_remote_turn(app, remote, uuid.clone(), historical_rebuild);
        }
        return;
    }

    // A tool update for a turn this terminal already settled must not
    // re-open a live card. The tool's committed row is printed and the
    // update has nothing to land on — `upsert_streaming_tool_use` would
    // re-create the card in the emptied overlay, and it would sit beside
    // the settled group card as a duplicate: three spinning Explore rows
    // under a "3 background agents launched" card. Post-turn agent
    // progress belongs to the tasks view, on a mirror as locally.
    if let Some(remote) = session.remote_background_attachment.as_ref() {
        if update_targets_settled_tool(remote, &params.update) {
            return;
        }
    }

    // The persisted transcript was already replayed before an initial attach.
    // Re-applying a historical reset would erase that fresh snapshot; only its
    // display-state reset and turn-overlay boundary are needed during rebuild.
    if historical_rebuild
        && matches!(
            &params.update,
            rebon_types::SessionUpdate::ContextReset { .. }
        )
    {
        app.rebon_tui.overlay.clear();
        app.plan_entries.clear();
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            remote.remote_hidden_tool_call_ids.clear();
            clear_remote_turn_projection(remote);
        }
        return;
    }

    let Some(remote) = session.remote_background_attachment.as_mut() else {
        return;
    };
    let effect = crate::tui::update::project_remote_session_update(
        app,
        params,
        &mut remote.remote_hidden_tool_call_ids,
    );
    if effect.overlay_cleared {
        remote.remote_hidden_tool_call_ids.clear();
        clear_remote_turn_projection(remote);
        return;
    }
    if let Some(text) = effect.text_delta {
        remote.current_turn_projected_text.push_str(&text);
        remote.awaiting_overlay_absorption = true;
    }
    if let Some(thinking) = effect.thinking_delta {
        remote.current_turn_projected_thinking.push_str(&thinking);
        remote.awaiting_overlay_absorption = true;
    }
    if let Some(tool_call_id) = effect.visible_tool_call_id {
        remote
            .current_turn_visible_tool_call_ids
            .insert(tool_call_id);
        remote.awaiting_overlay_absorption = true;
    }
}

/// Settle a watched turn the way a local turn ends: commit the overlay's
/// remaining tail to the transcript under a `partial-` uuid and remember
/// the projection, so the turn's persisted rows are covered instead of
/// spliced.
///
/// This is what keeps a long answer from printing twice. Its flushed slabs
/// are already in immutable scrollback under `partial-*` uuids the file
/// will never contain; replacing them with the persisted row is a second
/// print of the same text. Only called once the turn is over — settling on
/// a mid-turn absorption froze half-finished tool cards into scrollback
/// and reset the projection, which duplicated every tool turn. Returns
/// false — and commits nothing — when the projection does not provably
/// contain the persisted content (attached mid-turn, a lossy stream); the
/// turn then takes the splice path as before.
fn settle_watched_turn(
    app: &mut AppState,
    remote: &mut crate::background::RemoteBackgroundAttachment,
    turn: &PersistedRemoteTurn,
) -> bool {
    let Some(user_uuid) = remote.current_turn_user_uuid.clone() else {
        return false;
    };
    if !turn.boundary_found {
        return false;
    }
    let projected_tools = projected_tool_ids(remote);
    if !projection_covers_persisted_turn(
        turn,
        &remote.current_turn_projected_text,
        &remote.current_turn_projected_thinking,
        &projected_tools,
    ) {
        tracing::info!(
            target: "stream_dbg",
            %user_uuid,
            "mirror: watched turn could not settle locally; falling back to the splice"
        );
        return false;
    }
    // A turn with persisted rows already on screen must not settle: part of
    // it was spliced (the stream fell behind the file for a spell, so
    // `watching_live` wavered), and committing the overlay on top would
    // print the whole turn a second time — eight Read cards and then their
    // collapsed group card. One turn, one path: the splice finishes what it
    // started.
    let turn_uuids: HashSet<&str> = turn.assistant_uuids.iter().map(String::as_str).collect();
    let already_spliced = app
        .rebon_tui
        .transcript
        .rows()
        .iter()
        .any(|row| row.uuid().is_some_and(|uuid| turn_uuids.contains(uuid)));
    if already_spliced {
        tracing::info!(
            target: "stream_dbg",
            %user_uuid,
            "mirror: turn already partly spliced; leaving it to the splice"
        );
        return false;
    }
    tracing::info!(
        target: "stream_dbg",
        %user_uuid,
        covered = turn.assistant_uuids.len(),
        "mirror: settled watched turn from its own stream"
    );
    remote
        .covered_persisted_uuids
        .extend(turn.assistant_uuids.iter().cloned());
    commit_watched_turn_locally(app, remote, user_uuid);
    true
}

/// Commit the overlay as the watched turn's local representation: the tail
/// under a `partial-final` uuid, every `partial-*` row on screen marked
/// settled (the slabs this turn flushed, the tail just committed, earlier
/// turns' rows that were marked before), and the projection kept as the
/// snapshot late entries are covered against.
fn commit_watched_turn_locally(
    app: &mut AppState,
    remote: &mut crate::background::RemoteBackgroundAttachment,
    user_uuid: String,
) {
    let projected_tools = projected_tool_ids(remote);
    rebon_tui::reducer(
        &mut app.rebon_tui,
        rebon_tui::Action::FinalizeTurn {
            commit_uuid: format!("partial-final-{user_uuid}"),
            commit_timestamp: rebon_types::format_system_time_iso_ms(std::time::SystemTime::now()),
        },
    );
    let settled_rows = app
        .rebon_tui
        .transcript
        .rows()
        .iter()
        .filter_map(|row| row.uuid())
        .filter(|uuid| uuid.starts_with("partial-"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    remote.settled_local_row_uuids.extend(settled_rows);
    remote.last_settled_turn = Some(crate::background::SettledTurnProjection {
        user_uuid,
        assistant_text: std::mem::take(&mut remote.current_turn_projected_text),
        thinking_text: std::mem::take(&mut remote.current_turn_projected_thinking),
        tool_call_ids: projected_tools,
    });
}

fn clear_absorbed_streaming_overlay(
    app: &mut AppState,
    remote: &mut crate::background::RemoteBackgroundAttachment,
    absorbed: bool,
) {
    if absorbed {
        app.rebon_tui.overlay.clear();
        clear_remote_turn_projection(remote);
    }
}

fn refresh_remote_transcript(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    terminal_turn: bool,
) -> bool {
    let Some(remote) = session.remote_background_attachment.as_ref() else {
        return false;
    };
    let session_id = remote.session_id.clone();
    let cwd = remote.cwd.clone();
    let read = crate::session::transcript_replay::read_mirrored_transcript(
        session.server_state.as_ref(),
        &session_id,
        &cwd,
        remote.transcript_file_stat,
        remote.transcript_fingerprint != 0,
        terminal_turn,
    );
    let (transcript_stat, title, loaded_transcript) = match read {
        Ok(crate::session::transcript_replay::MirroredTranscriptRead::Unchanged) => return true,
        Ok(crate::session::transcript_replay::MirroredTranscriptRead::Loaded {
            file_stat,
            title,
            entries,
        }) => (file_stat, title, entries),
        Err(_) => return false,
    };
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.transcript_file_stat = transcript_stat;
    }
    let fingerprint = transcript_fingerprint(&loaded_transcript);
    let transcript_changed = session
        .remote_background_attachment
        .as_ref()
        .is_some_and(|remote| remote.transcript_fingerprint != fingerprint);
    // The turn boundary comes from the file (see the fn), and everything
    // below — absorption, withholding, the settle — reads it, so it must
    // be current before any of them run this refresh.
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        let boundary = last_remote_user_turn_uuid(&loaded_transcript);
        sync_turn_boundary_from_file(app, remote, boundary);
    }
    let absorbed = session
        .remote_background_attachment
        .as_ref()
        .is_some_and(|remote| {
            remote_turn_is_absorbed(&loaded_transcript, remote, fingerprint, terminal_turn)
        });

    // Coverage bookkeeping runs against the fresh record before the merge,
    // so rows the stream already committed to scrollback are covered by the
    // time the splice decides what is "new". Prune before extending: a
    // rewound entry's uuid must not linger in the covered set.
    let persisted_uuid_set = transcript_changed.then(|| {
        loaded_transcript
            .iter()
            .map(|entry| entry.uuid.clone())
            .collect::<HashSet<_>>()
    });
    // The turn in flight, while this terminal watches it live (projection
    // provably contains the file): its display is the overlay until the
    // turn is over, exactly like a local turn. Mid-turn its persisted rows
    // are withheld from the merge and absorption is suppressed — absorbing
    // mid-turn would commit a half-finished overlay (tool cards without
    // their results) and reset the projection, which is what made every
    // tool turn print twice. The whole turn settles locally once, at the
    // end. A turn not watched live keeps the old swap: mid-turn
    // absorption, splice.
    let mut withheld_turn_uuids: Vec<String> = Vec::new();
    let mut suppress_mid_turn_absorption = false;
    let mut keep_streaming_slabs = false;
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        if let Some(persisted) = persisted_uuid_set.as_ref() {
            prune_stale_coverage(remote, persisted);
        }
        cover_settled_turn_entries(remote, &loaded_transcript);
        if remote.awaiting_overlay_absorption {
            if let Some(user_uuid) = remote.current_turn_user_uuid.clone() {
                let turn = persisted_remote_turn(&loaded_transcript, &user_uuid);
                if turn.boundary_found {
                    let watching_live = projection_covers_persisted_turn(
                        &turn,
                        &remote.current_turn_projected_text,
                        &remote.current_turn_projected_thinking,
                        &projected_tool_ids(remote),
                    );
                    remote.current_turn_watched = watching_live;
                    let turn_over = terminal_turn || turn.next_user_found;
                    if watching_live && !turn_over {
                        withheld_turn_uuids = turn.assistant_uuids.clone();
                        suppress_mid_turn_absorption = true;
                        keep_streaming_slabs = true;
                    }
                    if !watching_live && transcript_changed {
                        // The dimension booleans are the diagnosis: a healthy
                        // watched turn that lands here is a predicate bug.
                        tracing::info!(
                            target: "stream_dbg",
                            text_ok = remote
                                .current_turn_projected_text
                                .starts_with(&turn.assistant_text),
                            tools_ok = turn
                                .tool_call_ids
                                .is_subset(&projected_tool_ids(remote)),
                            thinking_prefix_ok = remote
                                .current_turn_projected_thinking
                                .starts_with(&turn.thinking_text),
                            unprojectable = turn.has_unprojectable_content,
                            projected_text_len = remote.current_turn_projected_text.len(),
                            persisted_text_len = turn.assistant_text.len(),
                            "mirror: in-flight turn not watched live; the splice keeps it"
                        );
                    }
                    if watching_live && turn_over && absorbed {
                        settle_watched_turn(app, remote, &turn);
                    }
                }
            }
        }
    }
    let absorbed = absorbed && !suppress_mid_turn_absorption;

    if transcript_changed {
        let persisted_transcript_uuids = persisted_uuid_set.unwrap_or_default();
        let previous_persisted_uuids = session
            .remote_background_attachment
            .as_ref()
            .map(|remote| remote.persisted_transcript_uuids.clone())
            .unwrap_or_default();
        let mut refreshed = rebon_tui::AppState::default();
        let background_agent_tool_tasks =
            replay_transcript_entries_with_agent_tasks(&mut refreshed, loaded_transcript);
        let empty_covered = HashSet::new();
        let covered_persisted_uuids = session
            .remote_background_attachment
            .as_ref()
            .map(|remote| &remote.covered_persisted_uuids)
            .unwrap_or(&empty_covered);
        // Withheld rows are skipped the same way covered ones are, but only
        // for this refresh: the turn is still streaming in the overlay.
        let with_withheld;
        let skipped_persisted_uuids = if withheld_turn_uuids.is_empty() {
            covered_persisted_uuids
        } else {
            with_withheld = covered_persisted_uuids
                .iter()
                .cloned()
                .chain(withheld_turn_uuids.iter().cloned())
                .collect::<HashSet<_>>();
            &with_withheld
        };
        let empty_settled = HashSet::new();
        let settled_local_row_uuids = session
            .remote_background_attachment
            .as_ref()
            .map(|remote| &remote.settled_local_row_uuids)
            .unwrap_or(&empty_settled);
        let rows = merge_local_system_rows(
            refreshed.transcript.rows().to_vec(),
            app.rebon_tui.transcript.rows(),
            &previous_persisted_uuids,
            MergeStreamingContext {
                skipped_persisted_uuids,
                settled_local_row_uuids,
                keep_streaming_slabs,
            },
        );
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows);
        app.session_title = title;
        app.background_agent_tool_tasks = background_agent_tool_tasks;
        if let Some(remote) = session.remote_background_attachment.as_mut() {
            remote.transcript_fingerprint = fingerprint;
            remote.persisted_transcript_uuids = persisted_transcript_uuids;
        }
    }
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        clear_absorbed_streaming_overlay(app, remote, absorbed);
    }
    true
}

pub(super) fn sync_remote_tasks(
    app: &mut AppState,
    store: &crate::background::BackgroundStore,
    job_id: &str,
) {
    let Ok(snapshots) = crate::background::mirrored_task_snapshots_in_store(store, job_id) else {
        return;
    };
    app.remote_background_tasks.clear();
    for snapshot in snapshots {
        if let Some(tool_call_id) = snapshot.task.parent_tool_call_id.clone() {
            app.background_agent_tool_tasks.insert(
                tool_call_id,
                BackgroundAgentTaskRef::from_background_task(&snapshot.task),
            );
        }
        app.remote_background_tasks
            .insert(snapshot.task.task_id.clone(), snapshot);
    }
    synthesize_missing_async_launch_results(app);
}

fn synthesize_missing_async_launch_results(app: &mut AppState) {
    let by_tool_call = app
        .remote_background_tasks
        .values()
        .filter_map(|snapshot| {
            snapshot
                .task
                .parent_tool_call_id
                .as_deref()
                .map(|tool_call_id| (tool_call_id, &snapshot.task))
        })
        .collect::<std::collections::HashMap<_, _>>();
    if by_tool_call.is_empty() {
        return;
    }
    // Cheap pre-scan: cloning every transcript row is only worth it when some
    // Agent tool card actually lacks its async-launch output — the steady
    // state on every refresh tick is that none does.
    let needs_patch = app.rebon_tui.transcript.rows().iter().any(|row| {
        let rebon_tui::Message::Assistant(assistant) = row else {
            return false;
        };
        assistant.message.content.iter().any(|block| {
            let rebon_tui::AssistantContentBlock::ToolUse(tool) = block else {
                return false;
            };
            tool.name == "Agent"
                && tool.raw_output.is_none()
                && by_tool_call.contains_key(tool.id.as_str())
        })
    });
    if !needs_patch {
        return;
    }
    let mut rows = app.rebon_tui.transcript.rows().to_vec();
    let mut changed = false;
    for row in &mut rows {
        let rebon_tui::Message::Assistant(assistant) = row else {
            continue;
        };
        for block in &mut assistant.message.content {
            let rebon_tui::AssistantContentBlock::ToolUse(tool) = block else {
                continue;
            };
            if tool.name != "Agent" || tool.raw_output.is_some() {
                continue;
            }
            let Some(task) = by_tool_call.get(tool.id.as_str()) else {
                continue;
            };
            tool.raw_output = Some(serde_json::json!({
                "status": "async_launched",
                "task_id": task.task_id.clone(),
                "taskId": task.task_id.clone(),
                "agent_id": task.agent_id.as_deref().unwrap_or(&task.task_id),
                "agentId": task.agent_id.as_deref().unwrap_or(&task.task_id),
                "agent_type": task.agent_type.clone(),
                "agentType": task.agent_type.clone(),
                "tool_call_count": task.tool_use_count.unwrap_or(0),
                "toolCallCount": task.tool_use_count.unwrap_or(0),
            }));
            tool.status = Some(rebon_types::ToolCallStatus::Completed);
            changed = true;
        }
    }
    if changed {
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        refresh_remote_background_attachment, remote_endpoint_is_healthy, sync_remote_permission,
        synthesize_missing_async_launch_results,
    };
    use crate::tui::app::AppState;

    fn empty_runtime() -> rebon_session_host::BackgroundRuntimeFields {
        rebon_session_host::BackgroundRuntimeFields {
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

    fn system_message(uuid: &str, content: &str) -> rebon_tui::Message {
        rebon_tui::Message::System(rebon_tui::SystemMessage {
            uuid: uuid.into(),
            timestamp: "2026-07-17T00:00:00.000Z".into(),
            subtype: "info".into(),
            content: Some(content.into()),
            level: Some(rebon_tui::SystemLevel::Info),
            is_meta: None,
        })
    }

    /// The rule from the second hands-on round, kept: a worker somebody
    /// stopped on purpose is not brought back by whoever was watching. What
    /// changes is what the mirror does instead of letting go — it keeps the
    /// session on screen as the job's, every row of it, and says a prompt
    /// is the way on.
    #[test]
    fn a_stopped_worker_parks_the_mirror_instead_of_reviving_or_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = rebon_session_host::BackgroundStore::new(dir.path());
        let mut state = store
            .create_job(
                "prompt".into(),
                std::path::PathBuf::from("."),
                empty_runtime(),
            )
            .unwrap();
        state.process.status = rebon_session_host::BackgroundJobStatus::Stopped;
        state.identity.session_id = Some("sess-stopped".into());
        store.write_state(&state).unwrap();
        let job_id = state.job_id().to_string();
        let mut app = AppState::new();
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.attached_background_job_id = Some(job_id.clone());
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                job_id.clone(),
                "sess-stopped".into(),
                ".".into(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "gone".into(),
                },
            ));
        super::inject_system_message(&mut app, "local_command", "a row already on screen");
        let rows_before = app.rebon_tui.transcript.rows().len();
        app.is_loading = true;
        let mut pending_permission = None;

        assert!(super::recover_lost_worker(
            &mut app,
            &mut session,
            &mut pending_permission,
            &store
        ));

        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("the session stays parked on the job");
        assert!(!remote.is_live());
        assert_eq!(
            remote.status,
            rebon_session_host::BackgroundJobStatus::Stopped
        );
        assert_eq!(
            session.attached_background_job_id.as_deref(),
            Some(job_id.as_str())
        );
        assert!(!app.is_loading);
        assert_eq!(
            store.read_state(&job_id).unwrap().status(),
            rebon_session_host::BackgroundJobStatus::Stopped,
            "a stop is not undone by a watcher"
        );
        assert!(transcript_mentions(&app, "was stopped"));
        assert!(transcript_mentions(&app, "Type to continue"));
        assert_eq!(
            app.rebon_tui.transcript.rows().len(),
            rows_before + 1,
            "the rows on screen stay, plus the one line saying what happened"
        );
    }

    /// A parked mirror follows a worker the job was given elsewhere —
    /// another terminal's `rebon attach`, say — without starting one
    /// itself.
    #[test]
    fn a_parked_mirror_follows_a_worker_given_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let store = rebon_session_host::BackgroundStore::new(dir.path());
        let mut state = store
            .create_job(
                "prompt".into(),
                std::path::PathBuf::from("."),
                empty_runtime(),
            )
            .unwrap();
        state.process.status = rebon_session_host::BackgroundJobStatus::Stopped;
        state.identity.session_id = Some("sess-parked".into());
        store.write_state(&state).unwrap();
        let job_id = state.job_id().to_string();
        let mut app = AppState::new();
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        session.attached_background_job_id = Some(job_id.clone());
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                job_id.clone(),
                "sess-parked".into(),
                ".".into(),
                rebon_session_host::BackgroundJobStatus::Stopped,
                0,
            ),
        );
        let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(5);

        // Nothing to follow yet: the record names no worker.
        session
            .remote_background_attachment
            .as_mut()
            .unwrap()
            .last_worker_probe_at = long_ago;
        super::follow_worker_given_elsewhere(
            &mut app,
            &mut session,
            &store,
            std::time::Instant::now(),
        );
        assert!(!session
            .remote_background_attachment
            .as_ref()
            .unwrap()
            .is_live());

        // Somebody else gave the job a worker.
        state.process.status = rebon_session_host::BackgroundJobStatus::Idle;
        state.process.pid = Some(std::process::id());
        state.process.ipc_port = Some(1);
        state.process.ipc_token = Some("elsewhere".into());
        store.write_state(&state).unwrap();
        session
            .remote_background_attachment
            .as_mut()
            .unwrap()
            .last_worker_probe_at = long_ago;
        super::follow_worker_given_elsewhere(
            &mut app,
            &mut session,
            &store,
            std::time::Instant::now(),
        );

        let remote = session.remote_background_attachment.as_ref().unwrap();
        assert_eq!(
            remote.endpoint(),
            Some(crate::background::BackgroundIpcEndpoint {
                pid: std::process::id(),
                port: 1,
                token: "elsewhere".into(),
            })
        );
        assert!(transcript_mentions(&app, "Attached to worker"));
        assert!(
            store
                .read_events_tail(&job_id, 10)
                .unwrap()
                .iter()
                .all(|event| event.kind != "worker_revived_for_attach"),
            "following is not reviving"
        );
    }

    fn remote_task_snapshot(task_id: &str) -> rebon_session_host::BackgroundTaskSnapshot {
        rebon_session_host::BackgroundTaskSnapshot {
            task: rebon_session_host::BackgroundTaskDescriptor {
                task_id: task_id.into(),
                title: "Inspect remote session".into(),
                kind: "local_agent".into(),
                status: "running".into(),
                is_backgrounded: true,
                start_time_ms: 1,
                end_time_ms: None,
                last_progress: Some("reading source".into()),
                error: None,
                prompt: None,
                parent_tool_call_id: Some("tool-remote".into()),
                agent_id: Some(task_id.into()),
                agent_name: Some("Explore".into()),
                agent_type: Some("Explore".into()),
                model: None,
                token_count: Some(10),
                tool_use_count: Some(2),
                result: None,
            },
            updated_at_ms: 2,
            log_preview: vec!["agent started".into()],
            transcript: Vec::new(),
        }
    }

    fn assistant_row(uuid: &str) -> rebon_tui::Message {
        rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
            uuid: uuid.into(),
            timestamp: "2026-08-31T00:00:00.000Z".into(),
            message: rebon_tui::AssistantMessageInner {
                role: rebon_tui::AssistantRole::Assistant,
                content: Vec::new(),
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    #[test]
    fn remote_attachment_refresh_routes_updates_to_main_while_remote_child_is_foregrounded() {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let missing_job_id = format!("missing-remote-refresh-{}-{nonce}", std::process::id());
        let mut app = AppState::new();
        app.rebon_tui
            .transcript
            .push(system_message("s-main", "main marker"));
        app.remote_background_tasks
            .insert("agent-remote".into(), remote_task_snapshot("agent-remote"));
        let mut active_prompt = None;
        assert!(super::super::live_agent_view::switch_to_live_agent(
            &mut app,
            &mut active_prompt,
            "agent-remote",
        ));
        let child_rows = app.rebon_tui.transcript.rows().to_vec();
        let mut session = super::super::test_support::make_test_tui_session();
        let cwd = session.cwd.clone();
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                missing_job_id,
                "remote-main-session".into(),
                cwd,
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "missing-endpoint".into(),
                },
            ));
        let mut pending_permission = None;

        refresh_remote_background_attachment(&mut app, &mut session, &mut pending_permission);

        assert_eq!(app.foregrounded_task_id.as_deref(), Some("agent-remote"));
        assert_eq!(app.rebon_tui.transcript.rows(), child_rows.as_slice());
        let main = app.main_agent_view.as_ref().expect("saved main view");
        assert!(main.tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(message)
                if message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.contains("Lost worker"))
        )));
        // The record is unreadable, so there is no worker to follow — but
        // the session stays the job's, parked, rather than being dropped.
        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("parked on the job");
        assert!(!remote.is_live());
        assert!(app.remote_background_tasks.is_empty());

        super::super::live_agent_view::sync_foreground_agent_view(
            &mut app,
            session.engine_half.tasks.as_ref(),
        );

        assert_eq!(app.foregrounded_task_id, None);
        assert!(app.main_agent_view.is_none());
        assert!(app.rebon_tui.transcript.rows().iter().any(|row| matches!(
            row,
            rebon_tui::Message::System(message)
                if message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.contains("Lost worker"))
        )));
    }

    #[test]
    fn endpoint_probe_result_drives_remote_health_without_blocking_refresh() {
        for healthy in [true, false] {
            let mut session = super::super::test_support::make_test_tui_session();
            let mut remote = crate::background::RemoteBackgroundAttachment::new(
                "bg-probe".into(),
                "sess-probe".into(),
                session.cwd.clone(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "probe-token".into(),
                },
            );
            let worker = remote.worker.as_mut().expect("a live link");
            worker.last_endpoint_check_at = std::time::Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(healthy).unwrap();
            worker.endpoint_probe_rx = Some(rx);
            session.remote_background_attachment = Some(remote);

            assert_eq!(
                remote_endpoint_is_healthy(&mut session, std::time::Instant::now()),
                healthy
            );
            assert!(session
                .remote_background_attachment
                .as_ref()
                .and_then(|remote| remote.worker.as_ref())
                .expect("remote attachment")
                .endpoint_probe_rx
                .is_none());
        }
    }

    #[test]
    fn remote_permission_replaces_a_stale_endpoint_query_id() {
        let app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-permission".into(),
                "sess-permission".into(),
                session.cwd.clone(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: 1,
                    port: 1,
                    token: "endpoint-one".into(),
                },
            ));
        let mut pending_permission = None;
        let endpoint = session
            .remote_background_attachment
            .as_ref()
            .and_then(|remote| remote.endpoint())
            .expect("remote attachment")
            .clone();
        let snapshot = |query_id| rebon_session_host::BackgroundPermissionQuerySnapshot {
            query_id,
            turn_generation: 1,
            endpoint: Some(endpoint.clone()),
            tool: Some("Read".into()),
            tool_call_id: Some(format!("tool-{query_id}")),
            session_id: Some("sess-permission".into()),
            title: Some("Read files".into()),
            message: None,
            tool_input: None,
            metadata: None,
            options: Vec::new(),
        };

        sync_remote_permission(
            &app,
            &mut session,
            &mut pending_permission,
            Some(snapshot(11)),
        );
        assert_eq!(
            pending_permission
                .as_ref()
                .map(|pending| pending.outbound.id),
            Some(11)
        );

        sync_remote_permission(
            &app,
            &mut session,
            &mut pending_permission,
            Some(snapshot(29)),
        );
        assert_eq!(
            pending_permission
                .as_ref()
                .map(|pending| pending.outbound.id),
            Some(29)
        );
        assert_eq!(
            session
                .remote_background_attachment
                .as_ref()
                .and_then(|remote| remote.pending_permission_query_id),
            Some(29)
        );
    }

    #[test]
    fn remote_permission_replaces_the_same_query_id_after_endpoint_change() {
        let app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let old_endpoint = crate::background::BackgroundIpcEndpoint {
            pid: 1,
            port: 41001,
            token: "endpoint-one".into(),
        };
        let new_endpoint = crate::background::BackgroundIpcEndpoint {
            pid: 2,
            port: 41002,
            token: "endpoint-two".into(),
        };
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-permission-generation".into(),
                "sess-permission-generation".into(),
                session.cwd.clone(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                old_endpoint.clone(),
            ));
        let snapshot = |endpoint| rebon_session_host::BackgroundPermissionQuerySnapshot {
            query_id: 17,
            turn_generation: 1,
            endpoint: Some(endpoint),
            tool: Some("Read".into()),
            tool_call_id: Some("tool-17".into()),
            session_id: Some("sess-permission-generation".into()),
            title: Some("Read files".into()),
            message: None,
            tool_input: None,
            metadata: None,
            options: Vec::new(),
        };
        let mut pending_permission = None;
        sync_remote_permission(
            &app,
            &mut session,
            &mut pending_permission,
            Some(snapshot(old_endpoint)),
        );
        session
            .remote_background_attachment
            .as_mut()
            .unwrap()
            .link_worker(new_endpoint.clone());

        sync_remote_permission(
            &app,
            &mut session,
            &mut pending_permission,
            Some(snapshot(new_endpoint.clone())),
        );

        let remote = session.remote_background_attachment.as_ref().unwrap();
        assert_eq!(remote.pending_permission_query_id, Some(17));
        assert_eq!(remote.pending_permission_turn_generation, Some(1));
        assert_eq!(
            remote.pending_permission_endpoint.as_ref(),
            Some(&new_endpoint)
        );
        assert_eq!(
            pending_permission
                .as_ref()
                .map(|pending| pending.outbound.id),
            Some(17)
        );
    }

    #[test]
    fn remote_permission_replaces_the_same_query_and_endpoint_after_turn_change() {
        let app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let endpoint = crate::background::BackgroundIpcEndpoint {
            pid: 1,
            port: 41001,
            token: "persistent-endpoint".into(),
        };
        session.remote_background_attachment =
            Some(crate::background::RemoteBackgroundAttachment::new(
                "bg-permission-turn-generation".into(),
                "sess-permission-turn-generation".into(),
                session.cwd.clone(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                endpoint.clone(),
            ));
        let snapshot = |turn_generation| rebon_session_host::BackgroundPermissionQuerySnapshot {
            query_id: 17,
            turn_generation,
            endpoint: Some(endpoint.clone()),
            tool: Some("Read".into()),
            tool_call_id: Some(format!("tool-{turn_generation}")),
            session_id: Some("sess-permission-turn-generation".into()),
            title: Some("Read files".into()),
            message: None,
            tool_input: None,
            metadata: None,
            options: Vec::new(),
        };
        let mut pending_permission = None;
        sync_remote_permission(
            &app,
            &mut session,
            &mut pending_permission,
            Some(snapshot(1)),
        );

        sync_remote_permission(
            &app,
            &mut session,
            &mut pending_permission,
            Some(snapshot(2)),
        );

        let remote = session.remote_background_attachment.as_ref().unwrap();
        assert_eq!(remote.pending_permission_query_id, Some(17));
        assert_eq!(remote.pending_permission_turn_generation, Some(2));
        assert_eq!(remote.pending_permission_endpoint.as_ref(), Some(&endpoint));
    }

    #[test]
    fn synthesizes_async_launch_output_from_persisted_task_correlation() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(rebon_tui::Message::Assistant(rebon_tui::AssistantMessage {
                uuid: "a-agent".into(),
                timestamp: "2026-07-17T00:00:00.000Z".into(),
                message: rebon_tui::AssistantMessageInner {
                    role: rebon_tui::AssistantRole::Assistant,
                    content: vec![rebon_tui::AssistantContentBlock::ToolUse(
                        rebon_tui::AssistantToolUseBlock {
                            id: "tool-agent".into(),
                            name: "Agent".into(),
                            input: serde_json::json!({"description": "Inspect code"}),
                            tool_call_content: None,
                            raw_output: None,
                            title: None,
                            locations: None,
                            status: None,
                        },
                    )],
                },
                is_api_error_message: None,
                advisor_model: None,
                is_stream_continuation: None,
            })),
        );
        app.remote_background_tasks.insert(
            "agent-1".into(),
            rebon_session_host::BackgroundTaskSnapshot {
                task: rebon_session_host::BackgroundTaskDescriptor {
                    task_id: "agent-1".into(),
                    title: "Inspect code".into(),
                    kind: "local_agent".into(),
                    status: "running".into(),
                    is_backgrounded: true,
                    start_time_ms: 1,
                    end_time_ms: None,
                    last_progress: Some("reading files".into()),
                    error: None,
                    prompt: None,
                    parent_tool_call_id: Some("tool-agent".into()),
                    agent_id: Some("agent-1".into()),
                    agent_name: Some("Explore".into()),
                    agent_type: Some("Explore".into()),
                    model: None,
                    token_count: Some(10),
                    tool_use_count: Some(2),
                    result: None,
                },
                updated_at_ms: 2,
                log_preview: vec!["reading files".into()],
                transcript: Vec::new(),
            },
        );

        synthesize_missing_async_launch_results(&mut app);

        let rebon_tui::Message::Assistant(assistant) = &app.rebon_tui.transcript.rows()[0] else {
            panic!("expected assistant row");
        };
        let rebon_tui::AssistantContentBlock::ToolUse(tool) = &assistant.message.content[0] else {
            panic!("expected Agent tool");
        };
        assert_eq!(
            tool.raw_output
                .as_ref()
                .and_then(|output| output.get("status"))
                .and_then(serde_json::Value::as_str),
            Some("async_launched")
        );
        assert_eq!(
            tool.raw_output
                .as_ref()
                .and_then(|output| output.get("task_id"))
                .and_then(serde_json::Value::as_str),
            Some("agent-1")
        );
    }

    /// Invariant I4, written down as a test.
    ///
    /// The RFC states it as "permission mode, plan mode, model / effort /
    /// agent … all live on the owner; no client keeps an authoritative
    /// copy". The publish side always honoured it: the
    /// owner has been sending all of these on every change. The mirror read two
    /// fields of eighteen and went on showing its own start-up values for the
    /// rest, which is how a terminal came to name a model the worker had not
    /// used for hours, with nothing on screen to suggest otherwise.
    ///
    /// This list is the only place these fields are enumerated. The mirror
    /// stores the owner's snapshot whole, so a field the owner adds arrives
    /// with no edit anywhere — but a field that is supposed to be *shown* gets
    /// a line here, added on purpose. An enumeration in a test is a contract
    /// somebody has to think about; the same list copied into three appliers
    /// is three copies that rot apart, which is what happened.
    ///
    /// The second half matters as much as the first: a value that leaves the
    /// owner's answer must leave ours. Storing whole and replacing is what
    /// makes that true, and a mirror that patched instead would pass the first
    /// half of this test and fail the second.
    #[test]
    fn every_value_i4_names_reaches_the_mirror_and_none_of_them_go_stale() {
        let mut mirror = StreamedMirror::new();
        mirror.hello(1, 0);
        mirror.status(
            1,
            serde_json::json!({
                "permissionMode": "plan",
                "planMode": true,
                "model": "gpt-5.6-luna",
                "effort": "xhigh",
                "agent": "kernel:dsh",
            }),
        );
        mirror.refresh();

        let remote = mirror
            .session
            .remote_background_attachment
            .as_ref()
            .expect("attached");
        let owner = remote
            .owner
            .as_ref()
            .expect("the owner's answer is kept whole, not picked apart");
        assert_eq!(owner.permission_mode.as_deref(), Some("plan"));
        assert!(owner.plan_mode);
        assert_eq!(owner.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(owner.effort.as_deref(), Some("xhigh"));
        assert_eq!(
            owner.agent.as_deref(),
            Some("kernel:dsh"),
            "the fourth shared value: `None` from every worker until the router \
             was asked to publish it"
        );

        // The accessors readers actually call, not just the stored bytes.
        assert_eq!(remote.owner_model(), Some("gpt-5.6-luna"));
        assert_eq!(remote.owner_effort(), Some("xhigh"));

        // The owner changes its mind and stops naming two of them. A mirror
        // that patched would still be showing luna and xhigh; one that replaces
        // reports what the owner actually said, including that it said nothing.
        mirror.status(
            2,
            serde_json::json!({
                "permissionMode": "default",
                "planMode": false,
                "agent": "local",
            }),
        );
        mirror.refresh();

        let remote = mirror
            .session
            .remote_background_attachment
            .as_ref()
            .expect("attached");
        let owner = remote.owner.as_ref().expect("still stored whole");
        assert_eq!(owner.permission_mode.as_deref(), Some("default"));
        assert!(!owner.plan_mode);
        assert_eq!(owner.agent.as_deref(), Some("local"));
        assert_eq!(
            owner.model, None,
            "a value the owner stopped naming must not survive in the mirror"
        );
        assert_eq!(remote.owner_model(), None);
        assert_eq!(remote.owner_effort(), None);
    }

    /// The spinner, the clock and the "worked for" line follow the owner's
    /// `Turn` events, the way a local session's follow its prompt future.
    /// Nothing here reads the job record: the record's status trails the
    /// stream by a write, and reading it for the same fact is how the spinner
    /// came back for a tick after every turn ended.
    #[test]
    fn the_owners_turn_events_are_what_the_mirror_loads_and_stops_on() {
        let mut mirror = StreamedMirror::new();
        mirror.hello_idle(1, 0);
        mirror.refresh();
        assert!(mirror.turn_started_at().is_none(), "nothing runs yet");

        mirror.turn(1, rebon_session_host::TurnStreamState::Running, None);
        mirror.refresh();
        let started = mirror
            .turn_started_at()
            .expect("the owner announced a turn, so one is running");
        assert_eq!(mirror.app.prompt_completion_status, None);

        // A second `Running` — a reconnect replaying it — does not restart
        // the clock the user is watching.
        mirror.turn(2, rebon_session_host::TurnStreamState::Running, None);
        mirror.stream(3, StreamedMirror::chunk("半句"));
        mirror.refresh();
        assert_eq!(mirror.turn_started_at(), Some(started));
        assert_eq!(mirror.overlay(), "半句");

        mirror.turn(
            4,
            rebon_session_host::TurnStreamState::Idle,
            Some("end_turn"),
        );
        mirror.refresh();
        assert!(mirror.turn_started_at().is_none(), "the owner ended it");
        assert_eq!(
            mirror.app.prompt_completion_status,
            Some(crate::tui::app::PromptCompletionStatus::Succeeded)
        );
        assert_eq!(
            mirror.overlay(),
            "半句",
            "ending the turn does not throw the streamed text away; the transcript refresh lands it"
        );

        mirror.turn(5, rebon_session_host::TurnStreamState::Running, None);
        mirror.turn(
            6,
            rebon_session_host::TurnStreamState::Idle,
            Some("error: boom"),
        );
        mirror.refresh();
        assert_eq!(
            mirror.app.prompt_completion_status,
            Some(crate::tui::app::PromptCompletionStatus::Failed)
        );
    }

    /// A hello is the owner's whole answer at the moment a subscription opens
    /// — the attach, and every reconnect — and the one snapshot whose `busy`
    /// decides the turn: there is no `Turn` event before it on the link to be
    /// more current. A mirror attached mid-turn shows the spinner on the
    /// first frame the stream speaks, not on the next record read.
    #[test]
    fn a_hello_is_the_one_snapshot_whose_busy_decides_the_turn() {
        let mut mirror = StreamedMirror::new();
        mirror.hello_idle(1, 0);
        mirror.refresh();
        assert!(mirror.turn_started_at().is_none());

        mirror.hello(1, 0);
        mirror.refresh();
        assert!(
            mirror.turn_started_at().is_some(),
            "a hello that says busy is a turn in flight"
        );

        // A later status is published for other reasons — a mode changed, a
        // command answered — and carries the record's status, which lags. It
        // decides nothing about the turn.
        mirror.turn(
            1,
            rebon_session_host::TurnStreamState::Idle,
            Some("end_turn"),
        );
        mirror.status(2, serde_json::json!({ "busy": true }));
        mirror.refresh();
        assert!(
            mirror.turn_started_at().is_none(),
            "a status snapshot's busy does not restart a turn the stream ended"
        );
    }

    /// A permission the owner streams raises the dialog the frame it arrives,
    /// off the same event the record read would find a tick later.
    #[test]
    fn a_permission_the_owner_streams_raises_the_dialog_without_a_record_read() {
        let mut mirror = StreamedMirror::new();
        mirror.hello(1, 0);
        mirror.permission(1, 41);
        mirror.refresh();

        assert_eq!(
            mirror
                .pending_permission
                .as_ref()
                .map(|pending| pending.outbound.id),
            Some(41)
        );
        assert_eq!(
            mirror
                .session
                .remote_background_attachment
                .as_ref()
                .and_then(|remote| remote.pending_permission_query_id),
            Some(41)
        );

        // The owner's next status says nobody is waiting: another client
        // answered. The dialog goes with it.
        mirror.status(2, serde_json::json!({}));
        mirror.refresh();
        assert!(mirror.pending_permission.is_none());
    }

    /// The banner names the effort the owner runs its turns under, and its
    /// `None` is the owner's auto — not a stale level from this terminal's
    /// own start-up.
    #[test]
    fn the_owners_effort_is_what_the_banner_shows() {
        use rebon_types::ReasoningEffort;
        let mut mirror = StreamedMirror::new();
        mirror.app.effort_level = Some(ReasoningEffort::Low);
        mirror.hello(1, 0);
        mirror.status(1, serde_json::json!({ "effort": "xhigh" }));
        mirror.refresh();
        assert_eq!(mirror.app.effort_level, Some(ReasoningEffort::XHigh));

        mirror.status(2, serde_json::json!({}));
        mirror.refresh();
        assert_eq!(
            mirror.app.effort_level, None,
            "the owner's auto is auto here"
        );

        mirror.status(3, serde_json::json!({ "effort": "high" }));
        mirror.status(4, serde_json::json!({ "effort": "not-a-level" }));
        mirror.refresh();
        assert_eq!(
            mirror.app.effort_level,
            Some(ReasoningEffort::High),
            "a level this terminal cannot name leaves the last one it could"
        );
    }

    /// A mirror with a live stream, its job store, and a channel standing in
    /// for the owner's socket.
    struct StreamedMirror {
        _dir: tempfile::TempDir,
        store: rebon_session_host::BackgroundStore,
        job_id: String,
        app: AppState,
        session: crate::tui::wiring::TuiEngineSession,
        owner: std::sync::mpsc::Sender<rebon_session_host::SessionEvent>,
        /// The permission dialog the runner would be holding.
        pending_permission: Option<crate::tui::permission_modal::PendingPermission>,
    }

    impl StreamedMirror {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = rebon_session_host::BackgroundStore::new(dir.path());
            let state = store
                .create_job(
                    "prompt".into(),
                    std::path::PathBuf::from("."),
                    empty_runtime(),
                )
                .unwrap();
            let mut session = super::super::test_support::make_test_tui_session();
            let mut remote = crate::background::RemoteBackgroundAttachment::new(
                state.job_id().to_string(),
                "sess-live".into(),
                ".".into(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
                crate::background::BackgroundIpcEndpoint {
                    pid: std::process::id(),
                    port: 1,
                    token: "t".into(),
                },
            );
            let (owner, rx) = std::sync::mpsc::channel();
            remote.worker.as_mut().expect("a live link").events_rx = Some(rx);
            session.remote_background_attachment = Some(remote);
            Self {
                _dir: dir,
                store,
                job_id: state.identity.job_id,
                app: AppState::new(),
                session,
                owner,
                pending_permission: None,
            }
        }

        fn chunk(text: &str) -> rebon_types::SessionUpdateParams {
            rebon_types::SessionUpdateParams {
                session_id: "sess-live".into(),
                update: rebon_types::SessionUpdate::AgentMessageChunk {
                    content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                        text: text.into(),
                        annotations: None,
                    }),
                },
            }
        }

        fn user(uuid: &str) -> rebon_types::SessionUpdateParams {
            rebon_types::SessionUpdateParams {
                session_id: "sess-live".into(),
                update: rebon_types::SessionUpdate::QueuedUserMessage {
                    uuid: uuid.into(),
                    content: Vec::new(),
                    image_paste_ids: None,
                },
            }
        }

        /// The owner writes a line to its log, stamped as the stream numbered it.
        fn log_stamped(&self, epoch: u64, cursor: u64, params: rebon_types::SessionUpdateParams) {
            self.store
                .append_session_update_for_turn_at(
                    &self.job_id,
                    1,
                    &params,
                    rebon_session_host::StreamStamp { epoch, cursor },
                )
                .unwrap();
        }

        /// An owner from before stamps: the log line says nothing of the stream.
        fn log_unstamped(&self, params: rebon_types::SessionUpdateParams) {
            self.store
                .append_session_update(&self.job_id, &params)
                .unwrap();
        }

        fn hello(&self, epoch: u64, cursor: u64) {
            let status = serde_json::from_value::<rebon_session_host::SessionStatusSnapshot>(
                serde_json::json!({
                    "jobId": self.job_id,
                    "cwd": ".",
                    "status": serde_json::to_value(rebon_session_host::BackgroundJobStatus::Running).unwrap(),
                    "busy": true,
                    "turnGeneration": 1,
                    "updatedAtMs": 1,
                }),
            )
            .expect("a status");
            self.owner
                .send(rebon_session_host::SessionEvent::Hello {
                    cursor,
                    turn_generation: 1,
                    status: Box::new(status),
                    epoch,
                })
                .unwrap();
        }

        fn stream(&self, cursor: u64, params: rebon_types::SessionUpdateParams) {
            self.owner
                .send(rebon_session_host::SessionEvent::SessionUpdate {
                    cursor,
                    update: serde_json::to_value(params).unwrap(),
                })
                .unwrap();
        }

        /// The owner publishes a status. `extra` is merged over the minimum a
        /// snapshot needs, so a test names only the fields it is about.
        fn status(&self, cursor: u64, extra: serde_json::Value) {
            let mut body = serde_json::json!({
                "jobId": self.job_id,
                "cwd": ".",
                "status": serde_json::to_value(rebon_session_host::BackgroundJobStatus::Running)
                    .unwrap(),
                "busy": true,
                "turnGeneration": 1,
                "updatedAtMs": 1,
            });
            let (Some(body_map), Some(extra_map)) = (body.as_object_mut(), extra.as_object())
            else {
                panic!("both halves of a status are objects");
            };
            for (key, value) in extra_map {
                body_map.insert(key.clone(), value.clone());
            }
            let snapshot =
                serde_json::from_value::<rebon_session_host::SessionStatusSnapshot>(body)
                    .expect("a status");
            self.owner
                .send(rebon_session_host::SessionEvent::Status {
                    cursor,
                    snapshot: Box::new(snapshot),
                })
                .unwrap();
        }

        fn gap(&self, from: u64, to: u64) {
            self.owner
                .send(rebon_session_host::SessionEvent::Gap { from, to })
                .unwrap();
        }

        /// A hello from an owner with nothing running.
        fn hello_idle(&self, epoch: u64, cursor: u64) {
            let status = serde_json::from_value::<rebon_session_host::SessionStatusSnapshot>(
                serde_json::json!({
                    "jobId": self.job_id,
                    "cwd": ".",
                    "status": serde_json::to_value(rebon_session_host::BackgroundJobStatus::Idle).unwrap(),
                    "busy": false,
                    "turnGeneration": 1,
                    "updatedAtMs": 1,
                }),
            )
            .expect("a status");
            self.owner
                .send(rebon_session_host::SessionEvent::Hello {
                    cursor,
                    turn_generation: 1,
                    status: Box::new(status),
                    epoch,
                })
                .unwrap();
        }

        /// The owner announced a turn starting or ending.
        fn turn(
            &self,
            cursor: u64,
            state: rebon_session_host::TurnStreamState,
            stop_reason: Option<&str>,
        ) {
            self.owner
                .send(rebon_session_host::SessionEvent::Turn {
                    cursor,
                    state,
                    stop_reason: stop_reason.map(str::to_string),
                    stop_refused: None,
                })
                .unwrap();
        }

        /// The owner is waiting on a permission, and says so on the stream.
        fn permission(&self, cursor: u64, query_id: u64) {
            let endpoint = self
                .session
                .remote_background_attachment
                .as_ref()
                .and_then(|remote| remote.endpoint())
                .expect("a live link");
            let query = rebon_session_host::BackgroundPermissionQuerySnapshot {
                query_id,
                turn_generation: 1,
                endpoint: Some(endpoint),
                tool: Some("Read".into()),
                tool_call_id: Some(format!("tool-{query_id}")),
                session_id: Some("sess-live".into()),
                title: Some("Read files".into()),
                message: None,
                tool_input: None,
                metadata: None,
                options: Vec::new(),
            };
            self.owner
                .send(rebon_session_host::SessionEvent::Permission {
                    cursor,
                    query: Box::new(query),
                })
                .unwrap();
        }

        fn turn_started_at(&self) -> Option<std::time::Instant> {
            self.session
                .remote_background_attachment
                .as_ref()
                .and_then(|remote| remote.running_turn_started_at())
        }

        /// One refresh's worth: drain the stream, then pump against the log.
        fn refresh(&mut self) {
            let event_count = self
                .store
                .read_state(&self.job_id)
                .unwrap()
                .outcome
                .event_count;
            super::drain_owner_events(
                &mut self.app,
                &mut self.session,
                &mut self.pending_permission,
            );
            super::pump_remote_session_updates(
                &mut self.app,
                &mut self.session,
                &self.store,
                event_count,
            );
        }

        fn overlay(&self) -> String {
            self.app
                .rebon_tui
                .overlay
                .combined_streaming_text()
                .unwrap_or_default()
        }

        fn link(&self) -> &crate::background::WorkerLink {
            self.session
                .remote_background_attachment
                .as_ref()
                .unwrap()
                .worker
                .as_ref()
                .unwrap()
        }

        fn file_offset(&self) -> u64 {
            self.session
                .remote_background_attachment
                .as_ref()
                .unwrap()
                .live_events_offset
        }
    }

    /// The point of the stream: a delta the owner pushed is on screen the
    /// frame it arrives, and the event log was not read to get it.
    #[test]
    fn deltas_come_off_the_stream_without_a_file_read() {
        let mut mirror = StreamedMirror::new();
        mirror.refresh();
        let offset_after_first_scan = mirror.file_offset();

        mirror.hello(7, 0);
        mirror.stream(1, StreamedMirror::user("user-live"));
        mirror.stream(2, StreamedMirror::chunk("流上来的"));
        mirror.refresh();

        assert_eq!(mirror.overlay(), "流上来的");
        assert_eq!(mirror.link().mark.cursor(), 2);
        assert_eq!(mirror.file_offset(), offset_after_first_scan);
        assert_eq!(
            mirror.store.events_len(&mirror.job_id).unwrap(),
            offset_after_first_scan,
            "nothing was written to the log, and nothing was read from it"
        );
    }

    /// The first scan rebuilds the turn in flight from the log while the
    /// owner's replay of the same deltas is already waiting on the stream.
    /// The stamp is what tells them apart: the log's copy lands, the stream's
    /// is skipped, and only what came after the log is applied from the stream.
    #[test]
    fn a_delta_the_file_already_showed_is_not_applied_again_from_the_stream() {
        let mut mirror = StreamedMirror::new();
        mirror.log_stamped(7, 1, StreamedMirror::user("user-live"));
        mirror.log_stamped(7, 2, StreamedMirror::chunk("甲"));
        mirror.log_stamped(7, 3, StreamedMirror::chunk("乙"));
        mirror.hello(7, 0);
        mirror.stream(2, StreamedMirror::chunk("甲"));
        mirror.stream(3, StreamedMirror::chunk("乙"));
        mirror.stream(4, StreamedMirror::chunk("丙"));

        mirror.refresh();

        assert_eq!(mirror.overlay(), "甲乙丙");
        assert_eq!(mirror.link().mark.cursor(), 4);
    }

    /// The reverse order: the log was read before the owner said hello, so
    /// the read could not know which cursors it was applying. The hello
    /// catches up with what the file already showed in its numbering.
    #[test]
    fn a_file_read_before_the_hello_still_counts_against_the_stream() {
        let mut mirror = StreamedMirror::new();
        mirror.log_stamped(7, 1, StreamedMirror::user("user-live"));
        mirror.log_stamped(7, 2, StreamedMirror::chunk("甲"));
        mirror.refresh();
        assert_eq!(mirror.overlay(), "甲");

        mirror.hello(7, 0);
        mirror.stream(2, StreamedMirror::chunk("甲"));
        mirror.stream(3, StreamedMirror::chunk("乙"));
        mirror.refresh();

        assert_eq!(mirror.overlay(), "甲乙");
    }

    /// A gap: the owner's ring no longer reaches back to where this mirror
    /// got to. What it missed is in the log, and it is applied from there —
    /// before the stream's later deltas, not after them.
    #[test]
    fn a_gap_is_filled_from_the_file_before_the_stream_goes_on() {
        let mut mirror = StreamedMirror::new();
        mirror.refresh();
        mirror.hello(7, 0);
        mirror.stream(1, StreamedMirror::user("user-live"));
        mirror.stream(2, StreamedMirror::chunk("一"));
        mirror.refresh();
        assert_eq!(mirror.overlay(), "一");

        mirror.log_stamped(7, 3, StreamedMirror::chunk("二"));
        mirror.log_stamped(7, 4, StreamedMirror::chunk("三"));
        mirror.gap(2, 5);
        mirror.stream(5, StreamedMirror::chunk("四"));
        mirror.refresh();

        assert_eq!(mirror.overlay(), "一二三四");
        assert_eq!(mirror.link().mark.cursor(), 5);
        assert!(!mirror.link().catching_up());
    }

    /// An owner that does not stamp its log — one from before the field —
    /// still streams deltas, but they cannot be told from the log's. The log
    /// stays the source and the stream's copies are left alone.
    #[test]
    fn an_owner_that_does_not_stamp_keeps_the_file_as_the_source() {
        let mut mirror = StreamedMirror::new();
        mirror.refresh();
        mirror.hello(0, 0);
        mirror.log_unstamped(StreamedMirror::user("user-live"));
        mirror.log_unstamped(StreamedMirror::chunk("文件里的"));
        mirror.stream(1, StreamedMirror::user("user-live"));
        mirror.stream(2, StreamedMirror::chunk("文件里的"));
        mirror.refresh();

        assert_eq!(mirror.overlay(), "文件里的");
        assert!(!mirror.link().stream_delivers_deltas());
    }

    #[test]
    fn pump_remote_session_updates_streams_new_chunks_into_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let store = rebon_session_host::BackgroundStore::new(dir.path());
        let state = store
            .create_job(
                "prompt".into(),
                std::path::PathBuf::from("."),
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
                },
            )
            .unwrap();
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut remote = crate::background::RemoteBackgroundAttachment::new(
            state.job_id().to_string(),
            "sess-live".into(),
            ".".into(),
            rebon_session_host::BackgroundJobStatus::Running,
            0,
            rebon_session_host::BackgroundIpcEndpoint {
                pid: std::process::id(),
                port: 1,
                token: "t".into(),
            },
        );
        remote.live_events_offset = store.events_len(&state.job_id()).unwrap();
        session.remote_background_attachment = Some(remote);

        let append = |update| {
            store
                .append_session_update(
                    &state.job_id(),
                    &rebon_types::SessionUpdateParams {
                        session_id: "sess-live".into(),
                        update,
                    },
                )
                .unwrap();
        };
        append(rebon_types::SessionUpdate::QueuedUserMessage {
            uuid: "user-old".into(),
            content: Vec::new(),
            image_paste_ids: None,
        });
        append(rebon_types::SessionUpdate::AgentMessageChunk {
            content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                text: "历史流不应重放".into(),
                annotations: None,
            }),
        });
        append(rebon_types::SessionUpdate::Plan {
            entries: vec![rebon_types::PlanEntry {
                content: "remote plan".into(),
                priority: rebon_types::PlanEntryPriority::High,
                status: rebon_types::PlanEntryStatus::InProgress,
            }],
        });
        append(rebon_types::SessionUpdate::TokenUsage {
            input_tokens: 31,
            output_tokens: 41,
        });

        store
            .append_session_update(
                &state.job_id(),
                &rebon_types::SessionUpdateParams {
                    session_id: "sess-live".into(),
                    update: rebon_types::SessionUpdate::QueuedUserMessage {
                        uuid: "user-live".into(),
                        content: Vec::new(),
                        image_paste_ids: None,
                    },
                },
            )
            .unwrap();
        store
            .append_session_update(
                &state.job_id(),
                &rebon_types::SessionUpdateParams {
                    session_id: "sess-live".into(),
                    update: rebon_types::SessionUpdate::AgentMessageChunk {
                        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                            text: "正在检查 attach 流程".into(),
                            annotations: None,
                        }),
                    },
                },
            )
            .unwrap();
        // A different session's update must not leak into this attachment.
        store
            .append_session_update(
                &state.job_id(),
                &rebon_types::SessionUpdateParams {
                    session_id: "sess-other".into(),
                    update: rebon_types::SessionUpdate::AgentMessageChunk {
                        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                            text: "外部会话内容".into(),
                            annotations: None,
                        }),
                    },
                },
            )
            .unwrap();

        let event_count = store
            .read_state(&state.job_id())
            .unwrap()
            .outcome
            .event_count;
        assert!(super::pump_remote_session_updates(
            &mut app,
            &mut session,
            &store,
            event_count,
        ));

        let streamed = app
            .rebon_tui
            .overlay
            .combined_streaming_text()
            .unwrap_or_default();
        assert!(streamed.contains("正在检查 attach 流程"));
        assert!(!streamed.contains("历史流不应重放"));
        assert!(!streamed.contains("外部会话内容"));
        assert_eq!(app.plan_entries.len(), 1);
        assert_eq!(app.plan_entries[0].content, "remote plan");
        assert_eq!(app.streaming_token_count, 41);
        let offset = session
            .remote_background_attachment
            .as_ref()
            .unwrap()
            .live_events_offset;
        assert_eq!(offset, store.events_len(&state.job_id()).unwrap());

        // The unchanged durable count gates the event-log read and projection.
        assert!(!super::pump_remote_session_updates(
            &mut app,
            &mut session,
            &store,
            event_count,
        ));
        let after = app
            .rebon_tui
            .overlay
            .combined_streaming_text()
            .unwrap_or_default();
        assert_eq!(after.matches("正在检查 attach 流程").count(), 1);

        session
            .remote_background_attachment
            .as_mut()
            .unwrap()
            .remote_hidden_tool_call_ids
            .insert("hidden-old-turn".into());
        append(rebon_types::SessionUpdate::QueuedUserMessage {
            uuid: "user-next".into(),
            content: Vec::new(),
            image_paste_ids: None,
        });
        let next_event_count = store
            .read_state(&state.job_id())
            .unwrap()
            .outcome
            .event_count;
        assert!(super::pump_remote_session_updates(
            &mut app,
            &mut session,
            &store,
            next_event_count,
        ));
        let remote = session.remote_background_attachment.as_ref().unwrap();
        assert_eq!(remote.current_turn_user_uuid.as_deref(), Some("user-next"));
        assert!(remote.remote_hidden_tool_call_ids.is_empty());
        assert!(app.rebon_tui.overlay.is_empty());
        // The watched turn settled at the handoff: its streamed text is
        // committed under a `partial-final` row, not discarded for the
        // splice to print again under the new prompt.
        assert_eq!(
            app.rebon_tui
                .transcript
                .rows()
                .last()
                .and_then(|row| row.uuid()),
            Some("partial-final-user-live")
        );
        assert!(remote.last_settled_turn.is_some());
    }

    fn transcript_entry(
        entry_type: &str,
        uuid: &str,
        content: serde_json::Value,
    ) -> rebon_session::TranscriptEntry {
        rebon_session::TranscriptEntry {
            entry_type: entry_type.into(),
            uuid: uuid.into(),
            parent_uuid: None,
            timestamp: Some("2026-07-17T00:00:00.000Z".into()),
            raw: serde_json::json!({
                "type": entry_type,
                "uuid": uuid,
                "message": {
                    "role": entry_type,
                    "content": content,
                },
            }),
        }
    }

    fn remote_with_projected_turn(
        text: &str,
        tool_call_id: Option<&str>,
    ) -> crate::background::RemoteBackgroundAttachment {
        let mut remote = crate::background::RemoteBackgroundAttachment::new(
            "bg-overlay".into(),
            "sess-overlay".into(),
            ".".into(),
            rebon_session_host::BackgroundJobStatus::Running,
            0,
            crate::background::BackgroundIpcEndpoint {
                pid: 1,
                port: 1,
                token: "overlay-token".into(),
            },
        );
        remote.current_turn_user_uuid = Some("user-current".into());
        remote.current_turn_projected_text = text.into();
        if let Some(tool_call_id) = tool_call_id {
            remote
                .current_turn_visible_tool_call_ids
                .insert(tool_call_id.into());
        }
        remote.awaiting_overlay_absorption = true;
        remote.current_turn_watched = true;
        remote
    }

    /// A fresh hosted session has no boundary until its first prompt is on
    /// file — an immediately admitted prompt emits no
    /// `queued_user_message` (only queued/steered prompts and
    /// AskUserQuestion answers do) — so the sync must adopt it, or the
    /// whole watched-turn machinery stays inert exactly as it shipped.
    #[test]
    fn the_file_boundary_is_adopted_when_none_is_tracked() {
        let entries = vec![transcript_entry(
            "user",
            "user-first",
            serde_json::json!([{"type": "text", "text": "prompt"}]),
        )];
        let mut app = AppState::new();
        let mut remote = attachment_for("bg-fresh");
        assert!(remote.current_turn_user_uuid.is_none());

        super::sync_turn_boundary_from_file(
            &mut app,
            &mut remote,
            super::last_remote_user_turn_uuid(&entries),
        );

        assert_eq!(remote.current_turn_user_uuid.as_deref(), Some("user-first"));
        assert!(remote.current_turn_watched);
    }

    /// A new prompt on file while an older boundary is tracked is a turn
    /// handoff: the watched previous turn settles before anything clears.
    #[test]
    fn a_new_file_boundary_settles_the_previous_watched_turn() {
        let entries = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "assistant-current",
                serde_json::json!([{"type": "text", "text": "最终判断"}]),
            ),
            transcript_entry(
                "user",
                "user-next",
                serde_json::json!([{"type": "text", "text": "next prompt"}]),
            ),
        ];
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("最终判断".into()),
        );
        let mut remote = remote_with_projected_turn("最终判断", None);

        super::sync_turn_boundary_from_file(
            &mut app,
            &mut remote,
            super::last_remote_user_turn_uuid(&entries),
        );

        assert_eq!(remote.current_turn_user_uuid.as_deref(), Some("user-next"));
        assert_eq!(
            app.rebon_tui
                .transcript
                .rows()
                .last()
                .and_then(|row| row.uuid()),
            Some("partial-final-user-current")
        );
        assert!(remote.last_settled_turn.is_some());
    }

    /// The file boundary can land in the same refresh frame as a fast
    /// first token. The first boundary an attachment ever adopts must not
    /// clear the overlay — whatever is there is the new turn's own head.
    #[test]
    fn the_first_adopted_boundary_keeps_early_deltas() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("early head".into()),
        );
        let mut remote = attachment_for("bg-early");
        remote.current_turn_projected_text = "early head".into();
        remote.awaiting_overlay_absorption = true;

        super::begin_remote_turn(&mut app, &mut remote, "user-first".into(), false);

        assert_eq!(remote.current_turn_user_uuid.as_deref(), Some("user-first"));
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("early head")
        );
        assert_eq!(remote.current_turn_projected_text, "early head");
        assert!(remote.awaiting_overlay_absorption);
    }

    /// An unchanged boundary is a no-op: no clear, no settle, no reset.
    #[test]
    fn an_unchanged_file_boundary_leaves_the_turn_alone() {
        let entries = vec![transcript_entry(
            "user",
            "user-current",
            serde_json::json!([{"type": "text", "text": "prompt"}]),
        )];
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("half an answer".into()),
        );
        let mut remote = remote_with_projected_turn("half an answer", None);

        super::sync_turn_boundary_from_file(
            &mut app,
            &mut remote,
            super::last_remote_user_turn_uuid(&entries),
        );

        assert_eq!(
            remote.current_turn_user_uuid.as_deref(),
            Some("user-current")
        );
        assert!(remote.awaiting_overlay_absorption);
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("half an answer")
        );
        assert!(app.rebon_tui.transcript.is_empty());
    }

    /// A fast-follow prompt: the next turn's boundary arrives before the
    /// file confirms the watched turn ended. The turn settles at the
    /// handoff — its tail commits, the snapshot is kept — instead of being
    /// wiped into the splice and printed again under the new prompt.
    #[test]
    fn a_watched_turn_settles_at_handoff_to_the_next_prompt() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("最终判断：基本达到目标".into()),
        );
        let mut remote = remote_with_projected_turn("最终判断：基本达到目标", None);

        super::begin_remote_turn(&mut app, &mut remote, "user-next".into(), false);

        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(
            rows.last().and_then(|row| row.uuid()),
            Some("partial-final-user-current")
        );
        assert!(app.rebon_tui.overlay.combined_streaming_text().is_none());
        let settled = remote.last_settled_turn.as_ref().expect("snapshot kept");
        assert_eq!(settled.user_uuid, "user-current");
        assert_eq!(remote.current_turn_user_uuid.as_deref(), Some("user-next"));
        assert!(remote.current_turn_watched);
    }

    /// A replayed boundary during an attach rebuild must not commit the
    /// replayed overlay: the primed store already holds those turns' rows.
    #[test]
    fn a_historical_rebuild_boundary_does_not_settle_at_handoff() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("replayed text".into()),
        );
        let mut remote = remote_with_projected_turn("replayed text", None);

        super::begin_remote_turn(&mut app, &mut remote, "user-next".into(), true);

        assert!(app.rebon_tui.transcript.rows().is_empty());
        assert!(remote.last_settled_turn.is_none());
    }

    /// A turn the splice already touched keeps the splice: no handoff
    /// commit on top of rows the splice printed.
    #[test]
    fn an_unwatched_turn_does_not_settle_at_handoff() {
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("spliced text".into()),
        );
        let mut remote = remote_with_projected_turn("spliced text", None);
        remote.current_turn_watched = false;

        super::begin_remote_turn(&mut app, &mut remote, "user-next".into(), false);

        assert!(app.rebon_tui.transcript.rows().is_empty());
        assert!(remote.last_settled_turn.is_none());
    }

    #[test]
    fn overlay_absorption_uses_current_user_boundary_for_text_and_initial_attach() {
        let entries = vec![
            transcript_entry(
                "assistant",
                "assistant-old",
                serde_json::json!([{"type": "text", "text": "current partial"}]),
            ),
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "assistant-current",
                serde_json::json!([{"type": "text", "text": "current partial and done"}]),
            ),
        ];
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("current partial".into()),
        );
        let mut remote = remote_with_projected_turn("current partial", None);
        remote.initial_overlay_fingerprint = Some(7);

        assert!(!super::remote_turn_is_absorbed(&entries, &remote, 7, false,));
        assert!(super::remote_turn_is_absorbed(&entries, &remote, 8, false,));
        super::clear_absorbed_streaming_overlay(&mut app, &mut remote, true);
        assert!(app.rebon_tui.overlay.combined_streaming_text().is_none());
        assert!(!remote.awaiting_overlay_absorption);
    }

    /// Settling commits the overlay's remaining tail under a `partial-`
    /// uuid, covers the turn's persisted rows, and keeps the projection as
    /// the snapshot late entries are covered against.
    #[test]
    fn settling_a_watched_turn_commits_the_overlay_tail_and_covers_its_rows() {
        let entries = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "assistant-current",
                serde_json::json!([{"type": "text", "text": "current partial and done"}]),
            ),
        ];
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("and done".into()),
        );
        let mut remote = remote_with_projected_turn("current partial and done", None);
        let turn = super::persisted_remote_turn(&entries, "user-current");

        assert!(super::settle_watched_turn(&mut app, &mut remote, &turn));

        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(
            rows.last().and_then(|row| row.uuid()),
            Some("partial-final-user-current")
        );
        assert!(app.rebon_tui.overlay.combined_streaming_text().is_none());
        assert!(remote.covered_persisted_uuids.contains("assistant-current"));
        assert!(remote
            .settled_local_row_uuids
            .contains("partial-final-user-current"));
        let settled = remote.last_settled_turn.as_ref().expect("snapshot kept");
        assert_eq!(settled.user_uuid, "user-current");
        assert_eq!(settled.assistant_text, "current partial and done");
    }

    /// A turn that already has a persisted row on screen was partly
    /// spliced (the stream fell behind the file for a spell). Settling on
    /// top of that would print the turn twice — eight Read cards and then
    /// their group card — so the splice keeps the turn.
    #[test]
    fn a_partly_spliced_turn_does_not_settle_on_top_of_its_own_rows() {
        let entries = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "assistant-current",
                serde_json::json!([{"type": "text", "text": "current partial and done"}]),
            ),
        ];
        let mut app = AppState::new();
        // The spliced row from the wavering spell is already on screen.
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::Commit(assistant_row("assistant-current")),
        );
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("and done".into()),
        );
        let mut remote = remote_with_projected_turn("current partial and done", None);
        let turn = super::persisted_remote_turn(&entries, "user-current");

        assert!(!super::settle_watched_turn(&mut app, &mut remote, &turn));
        assert!(remote.covered_persisted_uuids.is_empty());
        assert!(remote.last_settled_turn.is_none());
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("and done")
        );
    }

    /// Attached mid-turn, the projection is a suffix of the persisted text,
    /// not a superset: the turn must not settle, and the overlay stays for
    /// the splice path to replace.
    #[test]
    fn a_projection_that_missed_the_turns_start_does_not_settle() {
        let entries = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "assistant-current",
                serde_json::json!([{"type": "text", "text": "current partial and done"}]),
            ),
        ];
        let mut app = AppState::new();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText("and done".into()),
        );
        let mut remote = remote_with_projected_turn("and done", None);
        let turn = super::persisted_remote_turn(&entries, "user-current");

        assert!(!super::settle_watched_turn(&mut app, &mut remote, &turn));
        assert!(remote.covered_persisted_uuids.is_empty());
        assert!(remote.last_settled_turn.is_none());
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("and done")
        );
    }

    /// End to end through the real projection and settle: a Bash card that
    /// completed with raw output on the stream keeps that output on the
    /// row the settle commits.
    #[test]
    fn a_settled_turn_keeps_tool_results_on_the_committed_card() {
        let mut app = AppState::new();
        let mut hidden = std::collections::HashSet::new();
        let params = |update: rebon_types::SessionUpdate| rebon_types::SessionUpdateParams {
            session_id: "sess-tool".into(),
            update,
        };
        crate::tui::update::project_remote_session_update(
            &mut app,
            params(rebon_types::SessionUpdate::ToolCall {
                tool_call_id: "tool-bash".into(),
                title: "Bash".into(),
                kind: rebon_types::ToolKind::Execute,
                status: rebon_types::ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            }),
            &mut hidden,
        );
        crate::tui::update::project_remote_session_update(
            &mut app,
            params(rebon_types::SessionUpdate::ToolCallUpdate {
                tool_call_id: "tool-bash".into(),
                status: Some(rebon_types::ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: Some(
                    [("output".to_string(), serde_json::json!("hello from bash"))]
                        .into_iter()
                        .collect(),
                ),
            }),
            &mut hidden,
        );

        let entries = vec![
            transcript_entry(
                "user",
                "user-current",
                serde_json::json!([{"type": "text", "text": "prompt"}]),
            ),
            transcript_entry(
                "assistant",
                "assistant-current",
                serde_json::json!([{
                    "type": "tool_use", "id": "tool-bash", "name": "Bash", "input": {}
                }]),
            ),
        ];
        let mut remote = remote_with_projected_turn("", Some("tool-bash"));
        let turn = super::persisted_remote_turn(&entries, "user-current");
        assert!(super::settle_watched_turn(&mut app, &mut remote, &turn));

        let committed_block = app.rebon_tui.transcript.rows().iter().find_map(|row| {
            let rebon_tui::Message::Assistant(assistant) = row else {
                return None;
            };
            assistant
                .message
                .content
                .iter()
                .find_map(|block| match block {
                    rebon_tui::AssistantContentBlock::ToolUse(tool) if tool.id == "tool-bash" => {
                        Some(tool.clone())
                    }
                    _ => None,
                })
        });
        let committed_block = committed_block.expect("committed tool block present");
        assert!(
            committed_block.raw_output.is_some(),
            "raw output must survive the settle commit"
        );
    }

    fn attachment_for(job_id: &str) -> crate::background::RemoteBackgroundAttachment {
        crate::background::RemoteBackgroundAttachment::new(
            job_id.into(),
            "sess-forward".into(),
            ".".into(),
            rebon_session_host::BackgroundJobStatus::Running,
            0,
            crate::background::BackgroundIpcEndpoint {
                pid: 1,
                port: 1,
                token: "forward-token".into(),
            },
        )
    }

    /// The owner's MCP snapshot arrives on the same stream as everything
    /// else a mirror must agree with it about, and an owner with nothing to
    /// say about MCP leaves the last word in place.
    #[test]
    fn the_owner_mcp_snapshot_is_taken_off_the_stream() {
        let mut app = AppState::new();
        let mut session = crate::tui::runner::test_support::make_test_tui_session();
        let mut remote = attachment_for("bg-mcp");
        let (tx, rx) = std::sync::mpsc::channel();
        remote.worker.as_mut().expect("a live link").events_rx = Some(rx);
        session.remote_background_attachment = Some(remote);

        let mcp = rebon_session_host::McpStatusSnapshot {
            loader: "ready".into(),
            client: "ready".into(),
            tools: vec![rebon_session_host::McpToolSnapshot {
                name: "mcp__fixture__ping".into(),
                tokens: 12,
            }],
            ..Default::default()
        };
        let status = |mcp: Option<rebon_session_host::McpStatusSnapshot>| {
            let mut json = serde_json::json!({
                "jobId": "bg-mcp",
                "cwd": ".",
                "status": serde_json::to_value(rebon_session_host::BackgroundJobStatus::Idle)
                    .unwrap(),
                "busy": false,
                "turnGeneration": 1,
                "updatedAtMs": 1,
            });
            if let Some(mcp) = mcp {
                json["mcp"] = serde_json::to_value(mcp).unwrap();
            }
            Box::new(
                serde_json::from_value::<rebon_session_host::SessionStatusSnapshot>(json)
                    .expect("a status"),
            )
        };
        tx.send(rebon_session_host::SessionEvent::Status {
            cursor: 1,
            snapshot: status(Some(mcp.clone())),
        })
        .unwrap();
        tx.send(rebon_session_host::SessionEvent::Status {
            cursor: 2,
            snapshot: status(None),
        })
        .unwrap();

        super::drain_owner_events(&mut app, &mut session, &mut None);

        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("still attached");
        assert_eq!(remote.owner_mcp.as_ref(), Some(&mcp));
    }

    fn transcript_mentions(app: &AppState, needle: &str) -> bool {
        app.rebon_tui.transcript.rows().iter().any(|row| {
            matches!(row, rebon_tui::Message::System(message)
                if message.content.as_deref().is_some_and(|c| c.contains(needle)))
        })
    }

    /// A command forwarded to the worker is only useful if its answer comes
    /// back, and a worker that dies holding the request has to say so by
    /// name — a prompt that looks ignored is the failure being avoided.
    #[test]
    fn forwarded_command_output_lands_in_the_transcript_and_frees_the_slot() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(rebon_session_host::CommandOutput {
            text: "Context: 42% used".into(),
            tone: "info".into(),
        }))
        .unwrap();
        let mut remote = attachment_for("bg-forward");
        remote.pending_command = Some(crate::background::PendingRemoteCommand {
            name: "context".into(),
            rx,
        });
        session.remote_background_attachment = Some(remote);

        super::relay_forwarded_command_output(&mut app, &mut session);

        assert!(transcript_mentions(&app, "Context: 42% used"));
        assert!(session
            .remote_background_attachment
            .as_ref()
            .expect("attachment")
            .pending_command
            .is_none());

        // The worker died holding the request: named failure, not silence.
        let (dead_tx, dead_rx) =
            std::sync::mpsc::channel::<Result<rebon_session_host::CommandOutput, String>>();
        drop(dead_tx);
        session
            .remote_background_attachment
            .as_mut()
            .expect("attachment")
            .pending_command = Some(crate::background::PendingRemoteCommand {
            name: "compact".into(),
            rx: dead_rx,
        });

        super::relay_forwarded_command_output(&mut app, &mut session);

        assert!(transcript_mentions(&app, "/compact"));
        assert!(session
            .remote_background_attachment
            .as_ref()
            .expect("attachment")
            .pending_command
            .is_none());
    }

    /// The mode the UI displays must be the mode the worker enforces. The
    /// first refresh only takes the attach-time baseline; a push that fails
    /// re-arms, because leaving the user believing a restriction is in force
    /// is the dangerous direction of wrong.
    #[test]
    fn permission_mode_takes_a_baseline_then_re_arms_after_a_failed_push() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.remote_background_attachment = Some(attachment_for("bg-mode"));

        super::sync_permission_mode_with_worker(&mut app, &mut session, None);

        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("attachment");
        assert_eq!(remote.synced_permission_mode, Some(app.permission_mode));
        assert!(
            remote.mode_sync_rx.is_none(),
            "the attach-time baseline must not push anything"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err("the attached worker was replaced".to_string()))
            .unwrap();
        {
            let remote = session
                .remote_background_attachment
                .as_mut()
                .expect("attachment");
            remote.mode_sync_rx = Some(rx);
            remote.synced_permission_mode = Some(rebon_permissions::PermissionMode::Plan);
        }

        super::sync_permission_mode_with_worker(&mut app, &mut session, None);

        assert!(transcript_mentions(
            &app,
            "the attached worker was replaced"
        ));
        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("attachment");
        assert!(remote.mode_sync_rx.is_none());
        assert_eq!(
            remote.synced_permission_mode, None,
            "a failed push must re-arm so the next refresh retries"
        );
    }

    /// Several TUIs can mirror one worker. The one that did not make the
    /// change has no other way to hear about it, so a mode published by the
    /// worker wins over what this UI happens to be showing — displaying a
    /// restriction the worker is not enforcing (or missing one it is) is
    /// exactly the failure this path exists to prevent.
    #[test]
    fn a_mode_changed_by_another_client_is_adopted_from_the_worker() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.remote_background_attachment = Some(attachment_for("bg-shared-mode"));
        app.set_permission_mode(rebon_permissions::PermissionMode::Default);

        // Baseline first, as the attach-time refresh would take it.
        super::sync_permission_mode_with_worker(&mut app, &mut session, Some("default"));
        assert_eq!(
            app.permission_mode,
            rebon_permissions::PermissionMode::Default
        );

        // Another client switched the worker to plan.
        super::sync_permission_mode_with_worker(&mut app, &mut session, Some("plan"));

        assert_eq!(app.permission_mode, rebon_permissions::PermissionMode::Plan);
        let remote = session
            .remote_background_attachment
            .as_ref()
            .expect("attachment");
        assert_eq!(
            remote.synced_permission_mode,
            Some(rebon_permissions::PermissionMode::Plan),
            "adopting must not look like a local change and bounce back at the worker"
        );
        assert!(remote.mode_sync_rx.is_none(), "adopting pushes nothing");
    }

    /// A local change still wins while it is being pushed: the published
    /// value is what the worker knew a moment ago, and adopting it would
    /// undo the user's keystroke.
    #[test]
    fn a_local_change_is_not_undone_by_the_value_it_is_replacing() {
        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.remote_background_attachment = Some(attachment_for("bg-local-wins"));
        app.set_permission_mode(rebon_permissions::PermissionMode::Default);
        super::sync_permission_mode_with_worker(&mut app, &mut session, Some("default"));

        // The user switches locally; the worker still publishes the old one.
        app.set_permission_mode(rebon_permissions::PermissionMode::Plan);
        super::sync_permission_mode_with_worker(&mut app, &mut session, Some("default"));

        assert_eq!(app.permission_mode, rebon_permissions::PermissionMode::Plan);
        assert!(
            session
                .remote_background_attachment
                .as_ref()
                .expect("attachment")
                .mode_sync_rx
                .is_some(),
            "the local change must be on its way to the worker"
        );
    }

    fn tool_content(text: &str) -> Vec<rebon_types::ToolCallContent> {
        vec![rebon_types::ToolCallContent::Content(
            rebon_types::RegularContent {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: text.into(),
                    annotations: None,
                }),
            },
        )]
    }

    /// What a turn's updates are, in the order the engine emits them: a tool
    /// that runs and reports, an answer in two chunks, and a second tool
    /// whose arrival seals the prefix before it. Shared by the two drivers
    /// below so the comparison is of the paths, not of the input.
    fn tool_turn_updates() -> Vec<rebon_types::SessionUpdate> {
        vec![
            rebon_types::SessionUpdate::ToolCall {
                tool_call_id: "toolu_1".into(),
                title: "Read".into(),
                kind: rebon_types::ToolKind::Read,
                status: rebon_types::ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
            rebon_types::SessionUpdate::ToolCallUpdate {
                tool_call_id: "toolu_1".into(),
                status: Some(rebon_types::ToolCallStatus::Completed),
                title: None,
                content: Some(tool_content("TOOL-RESULT-BODY")),
                locations: None,
                raw_output: None,
            },
            rebon_types::SessionUpdate::AgentMessageChunk {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "ANSWER-A ".into(),
                    annotations: None,
                }),
            },
            rebon_types::SessionUpdate::AgentMessageChunk {
                content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: "ANSWER-B".into(),
                    annotations: None,
                }),
            },
            rebon_types::SessionUpdate::ToolCall {
                tool_call_id: "toolu_2".into(),
                title: "Read".into(),
                kind: rebon_types::ToolKind::Read,
                status: rebon_types::ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
        ]
    }

    /// The content each committed row carries, named rather than rendered:
    /// enough to tell "the tool result landed" from "the answer landed" and
    /// to catch either one landing twice.
    fn row_marks(rows: &[rebon_tui::Message]) -> Vec<Vec<&'static str>> {
        rows.iter()
            .map(|row| {
                let body = format!("{row:?}");
                ["TOOL-RESULT-BODY", "ANSWER-A", "ANSWER-B", "ToolUse"]
                    .into_iter()
                    .filter(|needle| body.contains(needle))
                    .collect()
            })
            .filter(|marks: &Vec<&'static str>| !marks.is_empty())
            .collect()
    }

    /// Drive the turn through the local translator — a session hosted in
    /// this process — and report what it committed.
    fn local_turn_rows() -> Vec<Vec<&'static str>> {
        let mut app = AppState::new();
        for update in tool_turn_updates() {
            crate::tui::update::translate_session_update(
                &mut app,
                rebon_types::SessionUpdateParams {
                    session_id: "sess-local".into(),
                    update,
                },
            );
        }
        row_marks(app.rebon_tui.transcript.rows())
    }

    /// Drive the same turn through the mirror, the way it actually arrives:
    /// the worker streams each update and grows the transcript file as it
    /// goes, and the terminal refreshes in between.
    ///
    /// This is the asymmetry the hosted default was reverted over
    /// (RFC-0004 section 16.23, the two presentation-plane rows of the
    /// symptom table). A mirror that commits less than the local path
    /// leaves the content in an overlay that scrollback never receives;
    /// one that commits more prints it twice.
    #[test]
    fn a_mirrored_tool_turn_commits_what_the_local_path_commits() {
        let _env = crate::test_env::lock_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", home.path());

        let jobs = tempfile::tempdir().unwrap();
        let store = rebon_session_host::BackgroundStore::new(jobs.path());
        let state = store
            .create_job(
                "read it".into(),
                std::path::PathBuf::from("."),
                empty_runtime(),
            )
            .unwrap();

        let session_id = "sess-tool";
        let projects_root = rebon_session::default_projects_root();
        let transcript_path =
            rebon_session::ensure_session_file_path(&projects_root, ".", session_id).unwrap();

        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        let mut remote = crate::background::RemoteBackgroundAttachment::new(
            state.job_id().to_string(),
            session_id.into(),
            ".".into(),
            rebon_session_host::BackgroundJobStatus::Running,
            0,
            rebon_session_host::BackgroundIpcEndpoint {
                pid: std::process::id(),
                port: 1,
                token: "t".into(),
            },
        );
        remote.live_events_offset = store.events_len(&state.job_id()).unwrap();
        session.remote_background_attachment = Some(remote);

        let append = |update| {
            store
                .append_session_update(
                    &state.job_id(),
                    &rebon_types::SessionUpdateParams {
                        session_id: session_id.into(),
                        update,
                    },
                )
                .unwrap();
        };
        let pump = |app: &mut AppState, session: &mut crate::tui::wiring::TuiEngineSession| {
            let count = store
                .read_state(&state.job_id())
                .unwrap()
                .outcome
                .event_count;
            super::pump_remote_session_updates(app, session, &store, count);
        };

        let user_line = serde_json::json!({
            "type": "user",
            "uuid": "u1",
            "parentUuid": null,
            "timestamp": "2026-09-01T00:00:00.000Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "read it"}]},
        });
        let tool_use_line = serde_json::json!({
            "type": "assistant",
            "uuid": "a1",
            "parentUuid": "u1",
            "timestamp": "2026-09-01T00:00:01.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}
            ]},
        });
        let tool_result_line = serde_json::json!({
            "type": "user",
            "uuid": "u2",
            "parentUuid": "a1",
            "timestamp": "2026-09-01T00:00:02.000Z",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "TOOL-RESULT-BODY"}
            ]},
        });
        let answer_line = serde_json::json!({
            "type": "assistant",
            "uuid": "a2",
            "parentUuid": "u2",
            "timestamp": "2026-09-01T00:00:03.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "ANSWER-A ANSWER-B"}
            ]},
        });
        let second_tool_line = serde_json::json!({
            "type": "assistant",
            "uuid": "a3",
            "parentUuid": "a2",
            "timestamp": "2026-09-01T00:00:04.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {}}
            ]},
        });
        let write_file = |lines: &[&serde_json::Value]| {
            let mut body = String::new();
            for line in lines {
                body.push_str(&serde_json::to_string(line).unwrap());
                body.push('\n');
            }
            std::fs::write(&transcript_path, body).unwrap();
        };

        // The prompt is on file and the terminal mirrors it.
        write_file(&[&user_line]);
        append(rebon_types::SessionUpdate::QueuedUserMessage {
            uuid: "u1".into(),
            content: Vec::new(),
            image_paste_ids: None,
        });
        pump(&mut app, &mut session);
        super::refresh_remote_transcript(&mut app, &mut session, false);

        // Each update is streamed, the file catches up, and the terminal
        // refreshes — the interleaving a real turn has and a single
        // end-of-turn replay does not.
        let file_after = [
            vec![&user_line, &tool_use_line],
            vec![&user_line, &tool_use_line, &tool_result_line],
            vec![&user_line, &tool_use_line, &tool_result_line],
            vec![&user_line, &tool_use_line, &tool_result_line, &answer_line],
            vec![
                &user_line,
                &tool_use_line,
                &tool_result_line,
                &answer_line,
                &second_tool_line,
            ],
        ];
        for (update, lines) in tool_turn_updates().into_iter().zip(file_after) {
            append(update);
            pump(&mut app, &mut session);
            write_file(&lines);
            super::refresh_remote_transcript(&mut app, &mut session, false);
        }

        let mid_turn = row_marks(app.rebon_tui.transcript.rows());

        // The turn ends: the mirror settles it the way a local turn's
        // prompt future resolving settles one.
        super::refresh_remote_transcript(&mut app, &mut session, true);
        let settled = row_marks(app.rebon_tui.transcript.rows());

        match previous {
            Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }

        assert_eq!(
            mid_turn,
            local_turn_rows(),
            "mid-turn the mirror must have committed exactly what the local \
             path committed: the sealed tool card and the answer before the \
             second tool, and nothing twice"
        );
        // Settling adds the still-live tail (the second tool card) and takes
        // nothing back: scrollback cannot unprint a row.
        assert_eq!(
            settled[..mid_turn.len()],
            mid_turn[..],
            "settling must not rewrite rows already committed: {settled:?}"
        );
        assert_eq!(
            settled
                .iter()
                .filter(|marks| marks.contains(&"ANSWER-B"))
                .count(),
            1,
            "the answer must be committed exactly once: {settled:?}"
        );
        assert_eq!(
            settled
                .iter()
                .filter(|marks| marks.contains(&"TOOL-RESULT-BODY"))
                .count(),
            1,
            "the tool result must be committed exactly once: {settled:?}"
        );
    }

    /// Regression for RFC-0004 §16.24: a persisted row can arrive in front
    /// of another persisted row after a streaming slab. The later persisted
    /// row used to make the slab look non-trailing, so inline committed it;
    /// the next stat-changing refresh then replaced that committed slab and
    /// the cursor printed the suffix a second time.
    #[test]
    fn splice_fallback_never_rewrites_the_inline_committed_prefix() {
        let _env = crate::test_env::lock_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", home.path());

        let session_id = "sess-splice-inline";
        let projects_root = rebon_session::default_projects_root();
        let transcript_path =
            rebon_session::ensure_session_file_path(&projects_root, ".", session_id).unwrap();
        let user = serde_json::json!({
            "type": "user",
            "uuid": "u-splice",
            "parentUuid": null,
            "timestamp": "2026-09-05T00:00:00.000Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "go"}]},
        });
        let empty_head = serde_json::json!({
            "type": "assistant",
            "uuid": "a-head",
            "parentUuid": "u-splice",
            "timestamp": "2026-09-05T00:00:01.000Z",
            "message": {"role": "assistant", "content": []},
        });
        let authoritative_head = serde_json::json!({
            "type": "assistant",
            "uuid": "a-head",
            "parentUuid": "u-splice",
            "timestamp": "2026-09-05T00:00:01.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "authoritative "}
            ]},
        });
        let persisted_tail = serde_json::json!({
            "type": "assistant",
            "uuid": "a-tail",
            "parentUuid": "a-head",
            "timestamp": "2026-09-05T00:00:02.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "tail"}
            ]},
        });
        let write_file = |lines: &[&serde_json::Value]| {
            let body = lines
                .iter()
                .map(|line| serde_json::to_string(line).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::write(&transcript_path, body).unwrap();
        };

        let mut app = AppState::new();
        let mut session = super::super::test_support::make_test_tui_session();
        session.remote_background_attachment = Some(
            crate::background::RemoteBackgroundAttachment::without_worker(
                "bg-splice-inline".into(),
                session_id.into(),
                ".".into(),
                rebon_session_host::BackgroundJobStatus::Running,
                0,
            ),
        );
        write_file(&[&user, &empty_head, &persisted_tail]);
        assert!(super::refresh_remote_transcript(
            &mut app,
            &mut session,
            false
        ));
        let first_stat = session
            .remote_background_attachment
            .as_ref()
            .unwrap()
            .transcript_file_stat;

        let mut rows = app.rebon_tui.transcript.rows().to_vec();
        rows.insert(1, assistant_row("partial-1-0"));
        rows.push(assistant_row("a-persisted-after"));
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows);
        let remote = session.remote_background_attachment.as_mut().unwrap();
        remote
            .persisted_transcript_uuids
            .insert("a-persisted-after".into());
        remote.current_turn_user_uuid = Some("u-splice".into());
        remote.current_turn_projected_text = "tail".into();
        remote.awaiting_overlay_absorption = true;
        remote.current_turn_watched = true;

        let mut cursor = super::super::inline_commit_cursor::InlineCommitCursor::default();
        let pinned = super::super::event_loop_entry::remote_attachment_live_prefix(
            session.remote_background_attachment.as_ref(),
            app.rebon_tui.transcript.rows(),
        )
        .unwrap_or(app.rebon_tui.transcript.len());
        let before = cursor.prepare_batches_with_pinned_live_prefix(
            app.rebon_tui.transcript.rows(),
            1,
            pinned,
        );
        let printed_before = before
            .iter()
            .flat_map(|batch| &app.rebon_tui.transcript.rows()[batch.start_row..batch.end_row])
            .filter_map(|row| row.uuid().map(str::to_string))
            .collect::<Vec<_>>();
        for batch in before {
            cursor.mark_committed(&batch, app.rebon_tui.transcript.rows(), 1);
        }

        // A persisted row after the slab is what defeated the old trailing
        // local-row scan and let the partial row enter immutable scrollback.
        // This length-changing write then defeats the stat gate and makes the
        // projection ("tail") fail coverage against "authoritative tail".
        write_file(&[&user, &authoritative_head, &persisted_tail]);
        assert!(super::refresh_remote_transcript(
            &mut app,
            &mut session,
            false
        ));
        let remote = session.remote_background_attachment.as_ref().unwrap();
        assert_ne!(remote.transcript_file_stat, first_stat);
        assert!(!remote.current_turn_watched, "the splice fallback must run");

        let after = cursor.prepare_batches(app.rebon_tui.transcript.rows(), 2);
        let printed_after = after
            .iter()
            .flat_map(|batch| &app.rebon_tui.transcript.rows()[batch.start_row..batch.end_row])
            .filter_map(|row| row.uuid().map(str::to_string))
            .collect::<Vec<_>>();
        let mut printed = printed_before;
        printed.extend(printed_after);
        let authoritative = app
            .rebon_tui
            .transcript
            .rows()
            .iter()
            .filter_map(|row| row.uuid().map(str::to_string))
            .collect::<Vec<_>>();

        match previous {
            Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }

        assert_eq!(
            printed,
            authoritative,
            "inline scrollback must be exactly the converged store, with no rewritten partial row or reprinted suffix"
        );
        assert_eq!(authoritative, ["u-splice", "a-head", "a-tail"]);
    }
}
