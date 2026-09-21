//! Sending a session option to whoever owns the session.
//!
//! `/model` and `/effort` change what the *session* runs under, and in a
//! mirrored terminal the session is not this process's. Applying them here
//! would leave the terminal showing one model while the worker kept using
//! another — the disagreement the shared-state rule exists to stop, and the
//! dangerous direction of wrong, because the user believes the change landed.

use crate::tui::app::AppState;
use crate::tui::wiring::TuiEngineSession;

use super::transcript_messages::inject_system_message;

/// Route a session option to the owner when this terminal is mirroring one.
///
/// Returns `true` when the owner has been asked, meaning the caller must not
/// also apply it locally. `false` means this terminal *is* the owner and the
/// local path is the right one.
pub(super) fn route_session_option_to_owner(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    key: &str,
    value: &str,
) -> bool {
    let Some((job_id, session_id, endpoint, busy)) =
        session.remote_background_attachment.as_ref().map(|remote| {
            (
                remote.job_id.clone(),
                remote.session_id.clone(),
                remote.endpoint(),
                remote.pending_command.is_some(),
            )
        })
    else {
        return false;
    };
    // Still the session's owner, even with no worker up: the option is
    // the session's, and applying it here would set it on a session this
    // process never runs. The next worker builds the session from its
    // record; the change belongs in a prompt to that worker, or in config.
    let Some(endpoint) = endpoint else {
        inject_system_message(
            app,
            "error",
            &format!(
                "Worker {job_id} is stopped, so /{key} was not sent. Type a prompt to continue the session in a new worker first."
            ),
        );
        app.follow_transcript_tail = true;
        return true;
    };
    if busy {
        inject_system_message(
            app,
            "warning",
            &format!("Still waiting on the previous command to answer — /{key} was not sent."),
        );
        app.follow_transcript_tail = true;
        return true;
    }
    let pending = crate::background::spawn_remote_session_option(
        &job_id,
        &session_id,
        &endpoint,
        key.to_string(),
        value.to_string(),
    );
    if let Some(remote) = session.remote_background_attachment.as_mut() {
        remote.pending_command = Some(pending);
    }
    // No "asked the host…" line while the answer is in flight.
    //
    // The round trip is a local IPC call — milliseconds — so the note existed
    // just long enough to be flicker, and it announced an errand the user did
    // not ask to be told about. `/model` in a session this terminal owns prints
    // one line: what happened. A session it mirrors is supposed to be the same
    // session, so it prints one line too, and the answer below is that line.
    //
    // A round trip that does go slow is not silent either: the answer still
    // lands when it lands, and a failure says so by name.
    app.follow_transcript_tail = true;
    true
}
