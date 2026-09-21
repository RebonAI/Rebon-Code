//! The terminal's half of the update surface: the notice above the prompt and
//! the transcript `/update` prints into.
//!
//! Deciding anything about updates belongs to `rebon-plugin-updater` — the
//! check, the preferences, the sentences. What is left here is what only a
//! terminal has. Each pass of the event loop asks the plugin's seat whether
//! the startup check has answered; the runner no longer spawns it, and with
//! `plugins.updater.enabled = false` there is no seat to ask, so nothing
//! appears.

use std::sync::Arc;

use tokio::runtime::Handle;

use rebon_plugin_updater::{
    NoticeChange, UpdateCheckPoll, UpdateCheckSeat, UpdateCheckService, UpdateCommand,
    UpdateCommandOutcome,
};

use crate::tui::app::AppState;

use super::inject_local_command_feedback;

/// The plugin's update-check seat, or `None` when the plugin is off.
fn update_check_seat() -> Option<Arc<UpdateCheckSeat>> {
    rebon_harness::kernel_bootstrap::process_kernel()
        .context()
        .get::<UpdateCheckService>()
}

pub(super) fn drain_update_check(app: &mut AppState) {
    drain_update_check_with_feedback(app, false);
}

pub(super) fn drain_update_check_with_feedback(app: &mut AppState, user_requested: bool) {
    let Some(seat) = update_check_seat() else {
        return;
    };

    match seat.poll() {
        UpdateCheckPoll::Idle | UpdateCheckPoll::Pending => {}
        UpdateCheckPoll::Ready(Ok(result)) => {
            tracing::debug!(
                decision = ?result.decision,
                current_version = %result.current_version,
                latest_version = %result.latest_version,
                package_name = %result.package_name,
                source = %result.source,
                "rebon-cli: startup update check completed"
            );
            let outcome = rebon_plugin_updater::report_check_result(result);
            // The notice goes up whether or not anyone asked; the sentence is
            // only printed for someone who did. A startup check that finds
            // nothing says nothing.
            if let NoticeChange::Set(notice) = outcome.notice {
                app.set_update_notice(notice);
            }
            if user_requested {
                say(app, outcome.messages);
            }
        }
        UpdateCheckPoll::Ready(Err(err)) => {
            tracing::debug!(%err, "rebon-cli: update check failed");
            if user_requested {
                inject_local_command_feedback(
                    app,
                    "update",
                    &format!("Update check failed: {err}"),
                );
            }
        }
        UpdateCheckPoll::Cancelled => {
            tracing::debug!("rebon-cli: update check task ended without a result");
            if user_requested {
                inject_local_command_feedback(
                    app,
                    "update",
                    "Update check was cancelled before completing.",
                );
            }
        }
    }
}

pub(super) fn handle_update_command(app: &mut AppState, handle: &Handle, cmd: UpdateCommand) {
    let outcome = rebon_plugin_updater::run_update_command(cmd, app.update_notice.as_ref(), handle);
    apply_update_outcome(app, outcome);
}

/// Move the notice the way the plugin asked, then print what it wrote.
///
/// The notice first: a `/update check` that found something should have its
/// hint up in the same frame that reports it.
fn apply_update_outcome(app: &mut AppState, outcome: UpdateCommandOutcome) {
    match outcome.notice {
        NoticeChange::Keep => {}
        NoticeChange::Set(notice) => app.set_update_notice(notice),
        NoticeChange::Clear => {
            app.clear_update_notice();
        }
    }
    say(app, outcome.messages);
}

fn say(app: &mut AppState, messages: Vec<String>) {
    for message in messages {
        inject_local_command_feedback(app, "update", &message);
    }
}
