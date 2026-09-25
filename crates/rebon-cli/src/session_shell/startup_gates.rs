//! The startup preflight the local TUI runs before it has a session.
//!
//! Each gate here decides one thing — development channels, the config
//! file, the settings scan and the MCP approvals that read it, directory
//! trust, the kernel plugin runtime, credentials — and several of them
//! ask the person by opening a startup dialog. The asking is drawing and
//! stays in `crate::tui::startup_dialog`; the deciding is a session
//! concern and lives here, which is what `crates/REBON.md` means by
//! `rebon-cli/src/tui/` holding only the code that draws.

use crate::rebon_config::{
    check_primary_config_json, config_home_dir, is_directory_trusted, resolve_from_dir_with,
    save_directory_trust, ResolvedProvider, RuntimeOverride,
};
use crate::tui::{onboarding_dialog, preflight, startup_dialog};
use rebon_dialog::dev_channels::DevChannelsAction;
use rebon_dialog::invalid_config::InvalidConfigAction;
use rebon_dialog::invalid_settings::InvalidSettingsAction;

/// What the credentials gate found, kept so the first frame can name the
/// provider without a second config read.
pub(crate) enum StartupProvider {
    /// The rebon-config active provider `build_tui_session` will resolve.
    Config(Box<ResolvedProvider>),
    /// Credentials the build will find another way: an `ANTHROPIC_API_KEY`
    /// / `OPENAI_API_KEY` env var, or a config the build itself will refuse
    /// (a stale `activeCustomProvider`) — treated as present so the user
    /// sees the real error from the bootstrap rather than a confusing jump
    /// into onboarding.
    Elsewhere,
    /// `build_tui_session` would hit the `no model credentials found` bail.
    Missing,
}

impl StartupProvider {
    pub(crate) fn resolved(&self) -> Option<&ResolvedProvider> {
        match self {
            Self::Config(resolved) => Some(resolved),
            Self::Elsewhere | Self::Missing => None,
        }
    }
}

/// Matches the resolution order in `wiring::resolve_runtime_model`:
/// rebon-config active provider first, env-var fallback second.
pub(crate) fn startup_provider(
    config_dir: &std::path::Path,
    provider_override: Option<&str>,
) -> StartupProvider {
    match resolve_from_dir_with(config_dir, provider_override) {
        Ok(Some(resolved)) => return StartupProvider::Config(Box::new(resolved)),
        Ok(None) => {}
        Err(_) => return StartupProvider::Elsewhere,
    }
    if std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return StartupProvider::Elsewhere;
    }
    if std::env::var("OPENAI_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return StartupProvider::Elsewhere;
    }
    StartupProvider::Missing
}

/// The development-channel gate: a channel list passed on the command
/// line has to be accepted before it joins the run's channels.
pub(crate) fn run_development_channel_gate(
    overrides: &mut RuntimeOverride,
    startup_started: std::time::Instant,
) -> anyhow::Result<()> {
    if !overrides.development_channels.is_empty() {
        match startup_dialog::run_dev_channels_dialog(&overrides.development_channels)? {
            DevChannelsAction::Accept => {
                let dev_entries = overrides
                    .development_channels
                    .drain(..)
                    .map(|entry| entry.with_dev(true));
                overrides.channels.extend(dev_entries);
            }
            DevChannelsAction::Exit { code } | DevChannelsAction::Cancel { code } => {
                std::process::exit(code);
            }
        }
    }

    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: development channel gate completed"
    );
    Ok(())
}

/// The config-file gate: an unreadable primary `config.json` is offered
/// for reset before anything reads it. Returns the config home.
pub(crate) fn run_config_file_gate(
    startup_started: std::time::Instant,
) -> anyhow::Result<std::path::PathBuf> {
    let config_dir = config_home_dir();
    if let Err(failure) = check_primary_config_json(&config_dir) {
        match startup_dialog::run_invalid_config_dialog(&failure)? {
            InvalidConfigAction::Exit => std::process::exit(1),
            InvalidConfigAction::Reset => {
                let payload = serde_json::to_string_pretty(&failure.default_config)?;
                std::fs::create_dir_all(
                    failure.file_path.parent().unwrap_or(config_dir.as_path()),
                )?;
                std::fs::write(&failure.file_path, format!("{payload}\n"))?;
                std::process::exit(0);
            }
        }
    }

    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        config_dir = %config_dir.display(),
        "rebon startup: config file gate completed"
    );
    Ok(config_dir)
}

/// The settings scan and the MCP-server approvals that follow it. Both
/// read the same scan, which is why they are one stage. Returns the UI
/// config the scan resolved, and records the resolved mode on the run's
/// overrides.
pub(crate) fn run_settings_and_mcp_gates(
    overrides: &mut RuntimeOverride,
    config_dir: &std::path::Path,
    cwd: &std::path::Path,
    startup_started: std::time::Instant,
) -> anyhow::Result<crate::ui_config::ResolvedUiConfig> {
    let settings_scan =
        preflight::scan_settings_with_overrides(config_dir, cwd, &overrides.settings);
    let ui_config = match crate::ui_config::resolve_from_scan(overrides.ui_mode, &settings_scan) {
        Ok(config) => config,
        Err(err) => {
            settings_scan.errors.iter().for_each(|_| ());
            return Err(anyhow::anyhow!(err).context("invalid UI mode configuration"));
        }
    };
    overrides.ui_mode = Some(ui_config.mode);
    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        cwd = %cwd.display(),
        ui_mode = ?ui_config.mode,
        settings_errors = settings_scan.errors.len(),
        "rebon startup: settings scan completed"
    );
    if !settings_scan.errors.is_empty() {
        match startup_dialog::run_invalid_settings_dialog(&settings_scan.errors)? {
            InvalidSettingsAction::Exit => std::process::exit(1),
            InvalidSettingsAction::Continue => {}
        }
    } else {
        let pending_servers =
            preflight::pending_project_mcp_servers(cwd, &settings_scan.local_settings);
        if pending_servers.len() == 1 {
            let action = startup_dialog::run_mcp_server_approval_dialog(&pending_servers[0])?;
            preflight::apply_mcp_approval_action(cwd, action)?;
        } else if !pending_servers.is_empty() {
            let action = startup_dialog::run_mcp_server_multiselect_dialog(&pending_servers)?;
            preflight::apply_mcp_multiselect_action(cwd, action)?;
        }
    }

    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: settings and mcp approval gates completed"
    );
    Ok(ui_config)
}

/// Per-directory trust gate — blocks startup and exits if the
/// user declines trust.
pub(crate) fn run_trust_gate(
    cwd: &std::path::Path,
    startup_started: std::time::Instant,
) -> anyhow::Result<()> {
    if !is_directory_trusted(cwd) {
        match startup_dialog::run_trust_dialog(&cwd.display().to_string())? {
            startup_dialog::TrustDialogAction::Accept => {
                save_directory_trust(cwd);
            }
            startup_dialog::TrustDialogAction::Exit => {
                std::process::exit(1);
            }
        }
    }

    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: trust gate completed"
    );
    Ok(())
}

/// Kernel plugin runtime gate. Rebon does not fetch a Node runtime by itself, so a
/// machine that configured kernel plugins and has nothing to run them on gets asked
/// here rather than discovering later that the composition quietly did not exist.
/// `kernelPlugins` is opt-in, so this asks nothing of anyone who did not configure it.
pub(crate) async fn run_kernel_plugin_runtime_gate(
    startup_started: std::time::Instant,
) -> anyhow::Result<()> {
    if let Some(missing) = rebon_plugin_host::plugin_boot::missing_runtime_for_configured_plugins()
    {
        let version = rebon_node_runtime::PINNED_NODE_VERSION.to_string();
        match startup_dialog::run_node_runtime_dialog(missing.entries, &missing.reason, &version)? {
            startup_dialog::NodeRuntimeAction::Install => {
                if let Err(error) = crate::node_cmd::install_pinned_runtime().await {
                    // Not fatal: a session without kernel plugins is still a
                    // session, and the person is standing right here to read why.
                    eprintln!("Could not install Node {version}: {error:#}");
                    eprintln!("Starting without kernel plugins. `rebon node install` retries.");
                }
            }
            startup_dialog::NodeRuntimeAction::Continue => {
                tracing::info!("starting without kernel plugins at the user's choice");
            }
        }
    }

    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: kernel plugin runtime gate completed"
    );
    Ok(())
}

/// Credentials gate. `build_tui_session` bails with
/// `no model credentials found` when neither a rebon-config active
/// provider nor an `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` env var
/// is set. Catch that case before entering the bootstrap so we can
/// show the onboarding wizard instead of dumping an error and
/// exiting. When the user skips a CLI `--provider` override we
/// resolve without it; when they pass one we still try to resolve
/// so a typo'd provider flag errors out loudly (same behaviour as
/// `build_tui_session`) rather than silently onboarding.
/// With `plugins.onboarding.enabled` off there is no wizard to show, so
/// this is the same dead end the user reaches by closing it without
/// configuring anything: say what to do and stop.
pub(crate) async fn run_credentials_gate(
    overrides: &mut RuntimeOverride,
    ui_config: &mut crate::ui_config::ResolvedUiConfig,
    config_dir: &std::path::Path,
    startup_started: std::time::Instant,
) -> anyhow::Result<StartupProvider> {
    let mut startup_provider = startup_provider(config_dir, overrides.provider.as_deref());
    if matches!(startup_provider, StartupProvider::Missing)
        && !onboarding_dialog::wizard_available()
    {
        eprintln!(
            "rebon: no model credentials configured, and the onboarding plugin \
             is switched off. Set ANTHROPIC_API_KEY / OPENAI_API_KEY (with \
             optional OPENAI_BASE_URL), or turn `plugins.onboarding.enabled` \
             back on and re-run `rebon` for the setup wizard."
        );
        std::process::exit(1);
    }
    if matches!(startup_provider, StartupProvider::Missing) {
        // Run on a blocking thread: the wizard's OAuth path calls
        // `Handle::block_on` internally, which panics when invoked
        // from inside the async runtime driver.
        let onboarding_handle = tokio::runtime::Handle::current();
        let initial_ui_mode = ui_config.mode;
        let onboarding_outcome = tokio::task::spawn_blocking(move || {
            onboarding_dialog::run_startup_onboarding(onboarding_handle, initial_ui_mode)
        })
        .await??;
        match onboarding_outcome {
            onboarding_dialog::StartupOnboardingOutcome::Completed { ui_mode } => {
                ui_config.mode = ui_mode;
                overrides.ui_mode = Some(ui_mode);
                // The wizard wrote a provider; the first frame shows it.
                startup_provider =
                    self::startup_provider(config_dir, overrides.provider.as_deref());
            }
            onboarding_dialog::StartupOnboardingOutcome::Aborted => {
                eprintln!(
                    "rebon: no model credentials configured. Re-run `rebon` to \
                     open the setup wizard, or set ANTHROPIC_API_KEY / \
                     OPENAI_API_KEY (with optional OPENAI_BASE_URL) and try again."
                );
                std::process::exit(1);
            }
        }
    }

    // Expired-session notice. An invalidated refresh token is not a broken
    // install — the provider is configured, it just needs a new login — but
    // startup used to die on the refresher's error, dropping the user at a shell
    // prompt with nothing to act on. Spend the one refresh attempt here (the
    // resolver's own call then finds the rotated token on disk and does nothing)
    // and let a rejected grant become a line inside the session, where `/login`
    // lives. Only an OAuth-backed provider reaches this; an API-key provider
    // never sees the notice.
    if let Some(oauth) = startup_provider
        .resolved()
        .and_then(|resolved| resolved.oauth.clone())
    {
        if matches!(
            crate::rebon_config::check_startup_oauth(config_dir, &oauth).await,
            crate::rebon_config::OAuthStartupState::LoginRequired
        ) {
            overrides
                .startup_notices
                .push(expired_login_notice(oauth.provider));
        }
    }

    tracing::info!(
        elapsed_ms = startup_started.elapsed().as_millis() as u64,
        "rebon startup: credentials gate completed"
    );
    Ok(startup_provider)
}

/// The line a session opens with when its login needs signing in again.
///
/// The ChatGPT wording is the notice this used to print verbatim.
fn expired_login_notice(login: &crate::rebon_config::AccountLoginSpec) -> String {
    format!(
        "Your {} session has expired — run /login to re-authenticate. \
         Model requests fail until you do.",
        login.display_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expired_login_notice_names_the_login_that_expired() {
        assert_eq!(
            expired_login_notice(crate::rebon_config::account_login::codex_login()),
            "Your ChatGPT (Codex) session has expired — run /login to re-authenticate. \
             Model requests fail until you do."
        );
        let copilot =
            crate::rebon_config::account_login(crate::rebon_config::COPILOT_LOGIN_ID).unwrap();
        assert!(expired_login_notice(copilot).starts_with("Your GitHub Copilot session"));
    }
}
