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

use super::transcript_messages::inject_local_command_feedback_with_command;

/// The plugin's update-check seat, or `None` when the plugin is off.
fn update_check_seat() -> Option<Arc<UpdateCheckSeat>> {
    rebon_harness::kernel_bootstrap::process_kernel()
        .context()
        .get::<UpdateCheckService>()
}

pub(super) fn drain_update_check(app: &mut AppState) {
    drain_update_install(app);
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
                say(app, "/update check", outcome.messages);
            }
        }
        UpdateCheckPoll::Ready(Err(err)) => {
            tracing::debug!(%err, "rebon-cli: update check failed");
            if user_requested {
                inject_local_command_feedback_with_command(
                    app,
                    "update",
                    "/update check",
                    &format!("Update check failed: {err}"),
                );
            }
        }
        UpdateCheckPoll::Cancelled => {
            tracing::debug!("rebon-cli: update check task ended without a result");
            if user_requested {
                inject_local_command_feedback_with_command(
                    app,
                    "update",
                    "/update check",
                    "Update check was cancelled before completing.",
                );
            }
        }
    }
}

pub(super) fn handle_update_command(
    app: &mut AppState,
    handle: &Handle,
    cmd: UpdateCommand,
    command_line: &str,
) {
    if cmd == UpdateCommand::Install {
        let message = match update_check_seat() {
            Some(seat) if seat.start_install(handle) => "Checking for Rebon updates...",
            Some(_) => "An update is already in progress.",
            None => "Updates are unavailable because the updater plugin is disabled.",
        };
        say(app, command_line, vec![message.to_string()]);
        return;
    }
    let outcome = rebon_plugin_updater::run_update_command(cmd, app.update_notice.as_ref(), handle);
    apply_update_outcome(app, command_line, outcome);
}

fn drain_update_install(app: &mut AppState) {
    let Some(seat) = update_check_seat() else {
        return;
    };
    loop {
        use rebon_plugin_updater::seat::UpdateInstallPoll;
        match seat.poll_install() {
            UpdateInstallPoll::Idle | UpdateInstallPoll::Pending => return,
            UpdateInstallPoll::Progress(message) => say(app, "/update", vec![message]),
            UpdateInstallPoll::Ready(result) => {
                apply_update_outcome(
                    app,
                    "/update",
                    rebon_plugin_updater::command::report_install_result(result),
                );
            }
            UpdateInstallPoll::Cancelled => {
                say(
                    app,
                    "/update",
                    vec!["Update stopped before completing.".to_string()],
                );
            }
        }
    }
}

/// Move the notice the way the plugin asked, then print what it wrote.
///
/// The notice first: a `/update check` that found something should have its
/// hint up in the same frame that reports it.
fn apply_update_outcome(app: &mut AppState, command_line: &str, outcome: UpdateCommandOutcome) {
    match outcome.notice {
        NoticeChange::Keep => {}
        NoticeChange::Set(notice) => app.set_update_notice(notice),
        NoticeChange::Clear => {
            app.clear_update_notice();
        }
    }
    say(app, command_line, outcome.messages);
}

fn say(app: &mut AppState, command_line: &str, messages: Vec<String>) {
    inject_local_command_feedback_with_command(app, "update", command_line, &messages.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_plugin_updater::UpdateNoticeState;

    #[test]
    fn update_feedback_groups_output_under_the_typed_command() {
        let mut app = AppState::new();
        apply_update_outcome(
            &mut app,
            "/update status",
            UpdateCommandOutcome {
                messages: vec![
                    "Rebon 1.0.0 -> 1.1.0".into(),
                    "Run /update to install.".into(),
                ],
                notice: NoticeChange::Keep,
            },
        );
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let rebon_tui::Message::System(message) = &rows[0] else {
            panic!("expected local command feedback");
        };
        assert_eq!(message.subtype, "local_command");
        assert_eq!(
            message.content.as_deref(),
            Some("/update status\n⎿ Rebon 1.0.0 -> 1.1.0\n  Run /update to install.")
        );
    }

    #[test]
    fn installing_clears_the_notice_but_a_failure_keeps_it() {
        let mut app = AppState::new();
        let notice = UpdateNoticeState {
            current_version: "1.0.0".into(),
            latest_version: "1.1.0".into(),
            package_name: "@rebon/cli".into(),
            command: "npm install -g @rebon/cli@latest".into(),
            checked_at: std::time::SystemTime::UNIX_EPOCH,
        };
        app.set_update_notice(notice.clone());
        apply_update_outcome(
            &mut app,
            "/update",
            rebon_plugin_updater::command::report_install_result(Err(anyhow::anyhow!(
                "npm failed"
            ))),
        );
        assert_eq!(app.update_notice, Some(notice));
        apply_update_outcome(
            &mut app,
            "/update",
            rebon_plugin_updater::command::report_install_result(Ok(
                rebon_plugin_updater::install::ManualUpdateResult::Installed {
                    current_version: "1.0.0".into(),
                    latest_version: "1.1.0".into(),
                },
            )),
        );
        assert!(app.update_notice.is_none());
    }
}
