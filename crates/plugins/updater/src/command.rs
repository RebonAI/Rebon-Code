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
    Install,
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
        return Some(UpdateCommand::Install);
    }
    if !rest.starts_with(' ') {
        return None;
    }
    let args: Vec<&str> = rest.split_whitespace().collect();
    if args.is_empty() {
        return Some(UpdateCommand::Install);
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
        UpdateCommand::Install => {
            report_install_result(handle.block_on(crate::install::install_update(|_| {})))
        }
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
    report_check_result_for_installation(result, &detect_current_installation())
}

fn report_check_result_for_installation(
    result: crate::check::UpdateCheckResult,
    installation: &crate::updater::InstallationDetection,
) -> UpdateCommandOutcome {
    use crate::updater::update_decision::UpdateDecision;
    let message = match result.decision {
        UpdateDecision::Update { .. } => {
            return UpdateCommandOutcome {
                messages: vec![format!(
                    "Rebon {} -> {}\nUpdate available. {}",
                    result.current_version,
                    result.latest_version,
                    crate::installation::update_action(installation),
                )],
                notice: NoticeChange::Set(UpdateNoticeState {
                    current_version: result.current_version,
                    latest_version: result.latest_version,
                    package_name: result.package_name,
                    command: result.command,
                    checked_at: result.checked_at,
                }),
            };
        }
        UpdateDecision::AlreadyAtOrAboveLatest => {
            format!("Rebon {} is up to date.", result.current_version)
        }
        UpdateDecision::AutoUpdatesDisabled => "Update checks are disabled.".to_string(),
        UpdateDecision::NoCurrentVersion => {
            "Cannot check for updates: the current version is unknown.".to_string()
        }
        UpdateDecision::NoLatestVersion => {
            "Cannot check for updates: no published version was found.".to_string()
        }
        UpdateDecision::SkippedByMinimumVersion => format!(
            "The update notice for Rebon {} is skipped or dismissed.\n{}",
            result.latest_version,
            crate::installation::update_action(installation),
        ),
    };
    UpdateCommandOutcome::say(message)
}

pub fn report_install_result(
    result: anyhow::Result<crate::install::ManualUpdateResult>,
) -> UpdateCommandOutcome {
    match result {
        Ok(crate::install::ManualUpdateResult::Installed {
            current_version,
            latest_version,
        }) => UpdateCommandOutcome {
            messages: vec![format!(
                "Rebon {current_version} -> {latest_version} installed.\nRestart Rebon to use the new version; this session is still running {current_version}."
            )],
            notice: NoticeChange::Clear,
        },
        Ok(crate::install::ManualUpdateResult::UpToDate { current_version }) => {
            UpdateCommandOutcome {
                messages: vec![format!("Rebon {current_version} is up to date.")],
                notice: NoticeChange::Clear,
            }
        }
        Err(error) => UpdateCommandOutcome::say(format!("Update failed: {error:#}")),
    }
}

pub fn update_usage_text() -> &'static str {
    "Usage: /update [status|check|skip|channel <latest|stable>|auto <on|off|status>]\nWithout arguments, /update checks and installs npm updates. Use /update check to check only."
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
        "Automatic installation is not available yet. Preference saved as on; use /update for a manual npm update."
    } else {
        "Automatic installation preference: off."
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
    fn bare_update_is_not_a_status_only_command() {
        assert_ne!(parse_update_command("/update"), Some(UpdateCommand::Status));
        assert_ne!(
            parse_update_command("/update   "),
            Some(UpdateCommand::Status)
        );
    }

    #[test]
    fn update_command_parses_local_variants() {
        assert_eq!(
            parse_update_command("/update"),
            Some(UpdateCommand::Install)
        );
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
    fn a_found_update_reports_a_summary_and_a_notice() {
        let outcome = report_check_result_for_installation(
            UpdateCheckResult {
                current_version: "1.0.0".to_string(),
                latest_version: "1.1.0".to_string(),
                package_name: "@rebon/cli".to_string(),
                source: "https://registry.npmjs.org/@rebon%2fcli".to_string(),
                command: "npm install -g @rebon/cli@latest".to_string(),
                checked_at: SystemTime::UNIX_EPOCH,
                decision: UpdateDecision::Update {
                    target_version: "1.1.0".to_string(),
                },
            },
            &crate::updater::InstallationDetection {
                installation_type: crate::updater::InstallationType::NpmGlobal,
                evidence: "test npm installation".into(),
            },
        );
        assert_eq!(
            outcome.messages,
            vec!["Rebon 1.0.0 -> 1.1.0\nUpdate available. Run /update to check and install. Restart Rebon after updating.".to_string()]
        );
        assert_eq!(outcome.notice, NoticeChange::Set(notice()));
    }

    /// And a check that found nothing leaves no notice behind, so a session
    /// that was already current never gets a hint pinned above its prompt.
    #[test]
    fn a_current_install_reports_no_notice() {
        let outcome = report_check_result_for_installation(
            UpdateCheckResult {
                current_version: "1.1.0".to_string(),
                latest_version: "1.1.0".to_string(),
                package_name: "@rebon/cli".to_string(),
                source: "test".to_string(),
                command: "npm install -g @rebon/cli@latest".to_string(),
                checked_at: SystemTime::UNIX_EPOCH,
                decision: UpdateDecision::AlreadyAtOrAboveLatest,
            },
            &crate::updater::InstallationDetection {
                installation_type: crate::updater::InstallationType::NpmGlobal,
                evidence: "test npm installation".into(),
            },
        );
        assert_eq!(
            outcome.messages,
            vec!["Rebon 1.1.0 is up to date.".to_string()]
        );
        assert_eq!(outcome.notice, NoticeChange::Keep);
    }

    #[test]
    fn check_suppressions_and_failures_are_readable_without_debug_fields() {
        for (decision, expected) in [
            (UpdateDecision::AutoUpdatesDisabled, "Update checks are disabled."),
            (UpdateDecision::NoCurrentVersion, "Cannot check for updates: the current version is unknown."),
            (UpdateDecision::NoLatestVersion, "Cannot check for updates: no published version was found."),
            (UpdateDecision::SkippedByMinimumVersion, "The update notice for Rebon 1.1.0 is skipped or dismissed.\nUse the original installer to update this standalone installation."),
        ] {
            let outcome = report_check_result_for_installation(
                UpdateCheckResult {
                    current_version: "1.0.0".into(),
                    latest_version: "1.1.0".into(),
                    package_name: "@rebon/cli".into(),
                    source: "test".into(),
                    command: "npm install -g @rebon/cli@latest".into(),
                    checked_at: SystemTime::UNIX_EPOCH,
                    decision,
                },
                &crate::updater::InstallationDetection {
                    installation_type: crate::updater::InstallationType::Native,
                    evidence: "test standalone installation".into(),
                },
            );
            assert_eq!(outcome.messages, vec![expected]);
            assert_eq!(outcome.notice, NoticeChange::Keep);
        }
    }

    #[test]
    fn manual_update_results_distinguish_installation_from_the_running_version() {
        let outcome = report_install_result(Ok(crate::install::ManualUpdateResult::Installed {
            current_version: "1.0.0".to_string(),
            latest_version: "1.1.0".to_string(),
        }));
        assert_eq!(outcome.notice, NoticeChange::Clear);
        assert_eq!(outcome.messages, vec![
            "Rebon 1.0.0 -> 1.1.0 installed.\nRestart Rebon to use the new version; this session is still running 1.0.0."
        ]);

        let outcome = report_install_result(Ok(crate::install::ManualUpdateResult::UpToDate {
            current_version: "1.1.0".to_string(),
        }));
        assert_eq!(outcome.notice, NoticeChange::Clear);
        assert_eq!(outcome.messages, vec!["Rebon 1.1.0 is up to date."]);

        let outcome = report_install_result(Err(anyhow::anyhow!("npm exited with code 1")));
        assert_eq!(outcome.notice, NoticeChange::Keep);
        assert_eq!(
            outcome.messages,
            vec!["Update failed: npm exited with code 1"]
        );
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
            "Automatic installation is not available yet. Preference saved as on; use /update for a manual npm update."
        );
        assert_eq!(
            auto_install_feedback(false),
            "Automatic installation preference: off."
        );
        let detection = crate::updater::InstallationDetection {
            installation_type: crate::updater::InstallationType::Development,
            evidence: "test target/debug path".to_string(),
        };
        let auto_status = format_auto_install_status(&prefs, &detection);
        assert!(auto_status.contains("Automatic installation: unavailable (preference on)"));
        assert!(auto_status.contains("Installation: development"));
        assert!(auto_status.contains("Update and rebuild your development checkout"));
    }
}
