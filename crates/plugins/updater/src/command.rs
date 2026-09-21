//! `/update`: the whole command except drawing it.
//!
//! Parsing the line, deciding what it means, writing the preference and
//! composing the sentence the user reads all live here. What the front end
//! keeps is the two things only it has: the transcript to print into, and the
//! notice pinned above the prompt — which is why running a command returns
//! [`UpdateCommandOutcome`] (lines to print, and what to do with the notice)
//! rather than reaching for an `AppState`.
//!
//! The command is registered on the command seat by this plugin, so turning
//! the plugin off takes `/update` with it instead of leaving a command whose
//! every branch reaches a feature that is gone.

use rebon_config::UpdatePreferences;
use rebon_slash_commands::strip_command_prefix;

use crate::installation::{
    detect_current_installation, format_auto_install_status, format_update_status,
    UpdateNoticeState,
};

/// One parsed `/update` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCommand {
    Status,
    Check,
    Skip,
    Channel(String),
    Auto(Option<bool>),
    Invalid(String),
}

/// What the front end must do to its own update notice once the command has
/// run. `Keep` is the common answer; only a check that found something sets
/// one, and only `skip` clears one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeChange {
    Keep,
    Set(UpdateNoticeState),
    Clear,
}

/// The result of running one `/update` line: what to print, in order, and
/// what became of the notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCommandOutcome {
    pub messages: Vec<String>,
    pub notice: NoticeChange,
}

impl UpdateCommandOutcome {
    fn say(message: impl Into<String>) -> Self {
        Self {
            messages: vec![message.into()],
            notice: NoticeChange::Keep,
        }
    }
}

pub fn parse_update_command(text: &str) -> Option<UpdateCommand> {
    let rest = strip_command_prefix(text, "update")?;
    if rest.is_empty() {
        return Some(UpdateCommand::Status);
    }
    if !rest.starts_with(' ') {
        return None;
    }
    let args: Vec<&str> = rest.split_whitespace().collect();
    if args.is_empty() {
        return Some(UpdateCommand::Status);
    }
    Some(match args[0].to_ascii_lowercase().as_str() {
        "status" if args.len() == 1 => UpdateCommand::Status,
        "status" => UpdateCommand::Invalid("status takes no arguments".to_string()),
        "check" if args.len() == 1 => UpdateCommand::Check,
        "check" => UpdateCommand::Invalid("check takes no arguments".to_string()),
        "skip" if args.len() == 1 => UpdateCommand::Skip,
        "skip" => UpdateCommand::Invalid("skip takes no arguments".to_string()),
        "channel" if args.len() == 2 => UpdateCommand::Channel(args[1].to_ascii_lowercase()),
        "channel" => UpdateCommand::Invalid("channel requires exactly one value".to_string()),
        "auto" | "auto-install" if args.len() == 1 => UpdateCommand::Auto(None),
        "auto" | "auto-install" if args.len() == 2 => match args[1].to_ascii_lowercase().as_str() {
            "status" => UpdateCommand::Auto(None),
            "on" => UpdateCommand::Auto(Some(true)),
            "off" => UpdateCommand::Auto(Some(false)),
            other => UpdateCommand::Invalid(other.to_string()),
        },
        "auto" | "auto-install" => {
            UpdateCommand::Invalid("auto requires on, off, or status".to_string())
        }
        other => UpdateCommand::Invalid(other.to_string()),
    })
}

/// Run one parsed `/update`.
///
/// `notice` is the notice the front end is currently showing, read only; what
/// to do with it comes back in the outcome. `handle` is the runtime the check
/// runs on: the caller is the terminal's blocking runner, and the request
/// already carries its own three-second timeout. If that runner is ever moved
/// onto an async worker thread, `check` needs a receiver/drain path of its own
/// instead of blocking here.
pub fn run_update_command(
    cmd: UpdateCommand,
    notice: Option<&UpdateNoticeState>,
    handle: &tokio::runtime::Handle,
) -> UpdateCommandOutcome {
    match cmd {
        UpdateCommand::Status => UpdateCommandOutcome::say(format_update_status(notice)),
        UpdateCommand::Check => {
            let mut outcome = match handle.block_on(crate::check::check_for_update()) {
                Ok(result) => report_check_result(result),
                Err(err) => {
                    tracing::debug!(%err, "updater: user-requested update check failed");
                    UpdateCommandOutcome::say(format!("Update check failed: {err}"))
                }
            };
            outcome
                .messages
                .insert(0, "Checking for Rebon updates...".to_string());
            outcome
        }
        UpdateCommand::Skip => {
            let Some(notice) = notice else {
                return UpdateCommandOutcome::say(
                    "No update notice is visible to skip. Run /update check first.",
                );
            };
            let latest = notice.latest_version.clone();
            // The notice goes whether or not the preference can be written:
            // the user asked for it off the screen, and a failed write is
            // reported rather than silently keeping it up.
            let message = match rebon_config::load_update_preferences() {
                Ok(mut prefs) => {
                    apply_skip_update_preferences(&mut prefs, &latest);
                    match rebon_config::save_update_preferences(&prefs) {
                        Ok(()) => format!("Skipped Rebon update {latest}."),
                        Err(err) => format!("Failed to persist skipped update: {err}"),
                    }
                }
                Err(err) => format!("Failed to load update settings: {err}"),
            };
            UpdateCommandOutcome {
                messages: vec![message],
                notice: NoticeChange::Clear,
            }
        }
        UpdateCommand::Channel(channel) => {
            if crate::updater::channel::parse_channel(&channel).is_none() {
                return UpdateCommandOutcome::say(update_usage_text());
            }
            UpdateCommandOutcome::say(match rebon_config::load_update_preferences() {
                Ok(mut prefs) => {
                    apply_channel_update_preferences(&mut prefs, &channel);
                    match rebon_config::save_update_preferences(&prefs) {
                        Ok(()) => format!(
                            "Update channel set to {channel}; skipped/dismissed versions cleared."
                        ),
                        Err(err) => format!("Failed to save update channel: {err}"),
                    }
                }
                Err(err) => format!("Failed to load update settings: {err}"),
            })
        }
        UpdateCommand::Auto(value) => {
            UpdateCommandOutcome::say(match rebon_config::load_update_preferences() {
                Ok(mut prefs) => match value {
                    Some(enabled) => {
                        apply_auto_install_update_preferences(&mut prefs, enabled);
                        match rebon_config::save_update_preferences(&prefs) {
                            Ok(()) => auto_install_feedback(enabled).to_string(),
                            Err(err) => {
                                format!("Failed to save auto install update setting: {err}")
                            }
                        }
                    }
                    None => format_auto_install_status(&prefs, &detect_current_installation()),
                },
                Err(err) => format!("Failed to load update settings: {err}"),
            })
        }
        UpdateCommand::Invalid(_) => UpdateCommandOutcome::say(update_usage_text()),
    }
}

/// What a finished check says, and whether it leaves a notice behind. Shared
/// by `/update check` and by the front end draining the startup check, so the
/// two cannot drift into describing the same result differently.
pub fn report_check_result(result: crate::check::UpdateCheckResult) -> UpdateCommandOutcome {
    if matches!(
        result.decision,
        crate::updater::update_decision::UpdateDecision::Update { .. }
    ) {
        let message = format!(
            "Update available: {} {} (current {}). Manual update command (not run): {}",
            result.package_name, result.latest_version, result.current_version, result.command
        );
        UpdateCommandOutcome {
            messages: vec![message],
            notice: NoticeChange::Set(UpdateNoticeState {
                current_version: result.current_version,
                latest_version: result.latest_version,
                package_name: result.package_name,
                command: result.command,
                checked_at: result.checked_at,
            }),
        }
    } else {
        UpdateCommandOutcome::say(format!(
            "No visible update: current={}, selected latest={}, decision={:?}.",
            result.current_version, result.latest_version, result.decision
        ))
    }
}

pub fn update_usage_text() -> &'static str {
    "Usage: /update [status|check|skip|channel <latest|stable>|auto <on|off|status>]"
}

pub fn apply_skip_update_preferences(prefs: &mut UpdatePreferences, latest: &str) {
    prefs.skipped_version = Some(latest.to_string());
    prefs.dismissed_version = None;
    prefs.dismissed_at_ms = None;
}

pub fn apply_channel_update_preferences(prefs: &mut UpdatePreferences, channel: &str) {
    prefs.channel = Some(channel.to_string());
    // Channel changes intentionally clear stale suppressions from the previous channel.
    prefs.skipped_version = None;
    prefs.dismissed_version = None;
    prefs.dismissed_at_ms = None;
}

pub fn apply_auto_install_update_preferences(prefs: &mut UpdatePreferences, enabled: bool) {
    prefs.auto_install = enabled;
}

/// The settings dialog's half of `/update auto`: one toggle, persisted.
pub fn save_update_auto_install_setting(enabled: bool) -> anyhow::Result<()> {
    let mut prefs = rebon_config::load_update_preferences()?;
    apply_auto_install_update_preferences(&mut prefs, enabled);
    rebon_config::save_update_preferences(&prefs)
}

pub fn auto_install_feedback(enabled: bool) -> &'static str {
    if enabled {
        "Auto install updates: on. Preference saved; background runner registration is explicit via `rebon update service install`. Package installation remains inactive until installer support lands."
    } else {
        "Auto install updates: off."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::UpdateCheckResult;
    use crate::updater::update_decision::UpdateDecision;
    use std::time::SystemTime;

    fn notice() -> UpdateNoticeState {
        UpdateNoticeState {
            current_version: "1.0.0".to_string(),
            latest_version: "1.1.0".to_string(),
            package_name: "@rebon/cli".to_string(),
            command: "npm install -g @rebon/cli@latest".to_string(),
            checked_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn update_command_parses_local_variants() {
        assert_eq!(parse_update_command("/update"), Some(UpdateCommand::Status));
        assert_eq!(
            parse_update_command("/update status"),
            Some(UpdateCommand::Status)
        );
        assert_eq!(
            parse_update_command("/update check"),
            Some(UpdateCommand::Check)
        );
        assert_eq!(
            parse_update_command("/update skip"),
            Some(UpdateCommand::Skip)
        );
        assert_eq!(
            parse_update_command("/update auto"),
            Some(UpdateCommand::Auto(None))
        );
        assert_eq!(
            parse_update_command("/update auto status"),
            Some(UpdateCommand::Auto(None))
        );
        assert_eq!(
            parse_update_command("/update auto on"),
            Some(UpdateCommand::Auto(Some(true)))
        );
        assert_eq!(
            parse_update_command("/update auto off"),
            Some(UpdateCommand::Auto(Some(false)))
        );
        assert_eq!(parse_update_command("/updatex"), None);
        assert!(matches!(
            parse_update_command("/update check extra"),
            Some(UpdateCommand::Invalid(_))
        ));
        assert!(matches!(
            parse_update_command("/update status extra"),
            Some(UpdateCommand::Invalid(_))
        ));
        assert!(matches!(
            parse_update_command("/update skip extra"),
            Some(UpdateCommand::Invalid(_))
        ));
        assert!(matches!(
            parse_update_command("/update channel"),
            Some(UpdateCommand::Invalid(_))
        ));
        assert!(matches!(
            parse_update_command("/update auto maybe"),
            Some(UpdateCommand::Invalid(_))
        ));
        assert!(matches!(
            parse_update_command("/update auto on extra"),
            Some(UpdateCommand::Invalid(_))
        ));
        assert!(matches!(
            parse_update_command("/update auto status extra"),
            Some(UpdateCommand::Invalid(_))
        ));
        assert!(matches!(
            parse_update_command("/update wat"),
            Some(UpdateCommand::Invalid(_))
        ));
    }

    /// An unparseable channel is answered with the usage line and writes
    /// nothing, so a typo cannot move the channel a session is checking.
    #[test]
    fn an_unknown_channel_is_refused_with_the_usage_line() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime for the check branch");
        let outcome = run_update_command(
            UpdateCommand::Channel("nightly".to_string()),
            None,
            runtime.handle(),
        );
        assert_eq!(outcome.messages, vec![update_usage_text().to_string()]);
        assert_eq!(outcome.notice, NoticeChange::Keep);
    }

    /// `skip` with nothing on screen says so and leaves the notice alone —
    /// the branch that used to be an `else` on the front end's `take()`.
    #[test]
    fn skip_without_a_visible_notice_changes_nothing() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let outcome = run_update_command(UpdateCommand::Skip, None, runtime.handle());
        assert_eq!(
            outcome.messages,
            vec!["No update notice is visible to skip. Run /update check first.".to_string()]
        );
        assert_eq!(outcome.notice, NoticeChange::Keep);
    }

    /// A found update reads the same whether the startup check or `/update
    /// check` produced it, and carries the notice the prompt hint is built
    /// from.
    #[test]
    fn a_found_update_reports_one_line_and_a_notice() {
        let outcome = report_check_result(UpdateCheckResult {
            current_version: "1.0.0".to_string(),
            latest_version: "1.1.0".to_string(),
            package_name: "@rebon/cli".to_string(),
            source: "https://registry.npmjs.org/@rebon%2fcli".to_string(),
            command: "npm install -g @rebon/cli@latest".to_string(),
            checked_at: SystemTime::UNIX_EPOCH,
            decision: UpdateDecision::Update {
                target_version: "1.1.0".to_string(),
            },
        });
        assert_eq!(
            outcome.messages,
            vec!["Update available: @rebon/cli 1.1.0 (current 1.0.0). Manual update command (not run): npm install -g @rebon/cli@latest".to_string()]
        );
        assert_eq!(outcome.notice, NoticeChange::Set(notice()));
    }

    /// And a check that found nothing leaves no notice behind, so a session
    /// that was already current never gets a hint pinned above its prompt.
    #[test]
    fn a_current_install_reports_no_notice() {
        let outcome = report_check_result(UpdateCheckResult {
            current_version: "1.1.0".to_string(),
            latest_version: "1.1.0".to_string(),
            package_name: "@rebon/cli".to_string(),
            source: "test".to_string(),
            command: "npm install -g @rebon/cli@latest".to_string(),
            checked_at: SystemTime::UNIX_EPOCH,
            decision: UpdateDecision::AlreadyAtOrAboveLatest,
        });
        assert_eq!(
            outcome.messages,
            vec!["No visible update: current=1.1.0, selected latest=1.1.0, decision=AlreadyAtOrAboveLatest.".to_string()]
        );
        assert_eq!(outcome.notice, NoticeChange::Keep);
    }

    #[test]
    fn update_skip_preferences_set_visible_latest_and_clear_dismissal() {
        let mut prefs = UpdatePreferences {
            disabled: false,
            auto_install: true,
            channel: Some("latest".to_string()),
            skipped_version: Some("1.0.0".to_string()),
            dismissed_version: Some("2.0.0".to_string()),
            dismissed_at_ms: Some(123),
        };

        apply_skip_update_preferences(&mut prefs, "3.0.0");

        assert_eq!(prefs.skipped_version.as_deref(), Some("3.0.0"));
        assert_eq!(prefs.dismissed_version, None);
        assert_eq!(prefs.dismissed_at_ms, None);
        assert_eq!(prefs.channel.as_deref(), Some("latest"));
        assert!(prefs.auto_install);
    }

    #[test]
    fn update_channel_preferences_set_channel_and_clear_suppressions() {
        let mut prefs = UpdatePreferences {
            disabled: true,
            auto_install: true,
            channel: Some("latest".to_string()),
            skipped_version: Some("1.0.0".to_string()),
            dismissed_version: Some("2.0.0".to_string()),
            dismissed_at_ms: Some(456),
        };

        apply_channel_update_preferences(&mut prefs, "stable");

        assert!(prefs.disabled);
        assert_eq!(prefs.channel.as_deref(), Some("stable"));
        assert_eq!(prefs.skipped_version, None);
        assert_eq!(prefs.dismissed_version, None);
        assert_eq!(prefs.dismissed_at_ms, None);
        assert!(prefs.auto_install);
    }

    #[test]
    fn update_auto_install_preferences_toggle_preserves_other_fields() {
        let mut prefs = UpdatePreferences {
            disabled: true,
            auto_install: false,
            channel: Some("stable".to_string()),
            skipped_version: Some("1.0.0".to_string()),
            dismissed_version: Some("2.0.0".to_string()),
            dismissed_at_ms: Some(456),
        };

        apply_auto_install_update_preferences(&mut prefs, true);

        assert!(prefs.auto_install);
        assert!(prefs.disabled);
        assert_eq!(prefs.channel.as_deref(), Some("stable"));
        assert_eq!(prefs.skipped_version.as_deref(), Some("1.0.0"));
        assert_eq!(prefs.dismissed_version.as_deref(), Some("2.0.0"));
        assert_eq!(prefs.dismissed_at_ms, Some(456));
        assert_eq!(
            auto_install_feedback(true),
            "Auto install updates: on. Preference saved; background runner registration is explicit via `rebon update service install`. Package installation remains inactive until installer support lands."
        );
        assert_eq!(auto_install_feedback(false), "Auto install updates: off.");
        let detection = crate::updater::InstallationDetection {
            installation_type: crate::updater::InstallationType::Development,
            evidence: "test target/debug path".to_string(),
        };
        let auto_status = format_auto_install_status(&prefs, &detection);
        assert!(auto_status.contains("Auto install updates: on."));
        assert!(auto_status.contains("installationSource: development"));
        assert!(auto_status.contains("autoInstallSupported: no"));
    }
}
