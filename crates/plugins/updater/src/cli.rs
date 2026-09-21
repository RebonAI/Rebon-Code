//! The `rebon update …` subcommands, and the scheduler registration the
//! background supervisor shares with them.
//!
//! These are clap subcommands, parsed before the kernel exists, so they are
//! not commands on a seat and cannot be — the binary has to answer `rebon
//! update status` without booting a plugin registry first. What *can* live
//! here is everything after the match arm: the entry point names a branch
//! and prints nothing of its own, and the code that knows about scheduler
//! specs, lock files and persisted dry-run state lives next to the code
//! that decides them.
//!
//! Two scheduler registrations run through here, and they are different
//! things: `rebon update service …` registers the *update* runner (still
//! dry-run only), while `rebon bg install` / `rebon agents service …`
//! registers the *background supervisor*. They share this module because they
//! share [`crate::updater::update_service`] — one per-user scheduler
//! abstraction, two task names — and splitting them apart would mean two
//! copies of "how do you register a per-user task on this platform".

use std::path::Path;

use clap::Subcommand;

use crate::installation::{detect_current_installation, format_headless_update_status};
use crate::updater::{
    install_service, service_install_spec, service_registration_status, service_uninstall_spec,
    supervisor_service_install_spec, supervisor_service_registration_status,
    supervisor_service_uninstall_spec, uninstall_service, PersistedUpdateState,
    RealServiceCommandRunner, RealServiceFileSystem, ServicePlatform, ServiceRegistrationStatus,
    UpdateLock, UpdateLockError,
};

/// `rebon update`.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum UpdateCliCommand {
    /// Print local update preferences and installation-source detection.
    Status,
    /// Real per-user scheduler registration commands; run-once remains dry-run/status-only.
    Service {
        #[command(subcommand)]
        command: UpdateServiceCommand,
    },
}

/// `rebon update service`.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum UpdateServiceCommand {
    /// Print persisted dry-run service state and local detection.
    Status,
    /// Acquire the update lock, record a dry-run state, and exit.
    RunOnce,
    /// Register per-user scheduler execution for dry-run run-once.
    Install,
    /// Remove Rebon per-user scheduler registration/artifacts.
    Uninstall,
}

/// Run one `rebon update …`.
pub async fn run_update_cli_command(command: UpdateCliCommand) -> anyhow::Result<()> {
    match command {
        UpdateCliCommand::Status => {
            let prefs = rebon_config::load_update_preferences()?;
            let installation = detect_current_installation();
            println!("{}", format_headless_update_status(&prefs, installation));
            Ok(())
        }
        UpdateCliCommand::Service { command } => run_update_service_command(command).await,
    }
}

async fn run_update_service_command(command: UpdateServiceCommand) -> anyhow::Result<()> {
    let config_home = rebon_config::config_home_dir();
    let state_path = crate::updater::update_state_path(&config_home);
    let lock_path = crate::updater::update_lock_path(&config_home);
    match command {
        UpdateServiceCommand::Status => {
            let prefs = rebon_config::load_update_preferences()?;
            let installation = detect_current_installation();
            println!("updateService: dry-run/status-only (package installation inactive)");
            println!("statePath: {}", state_path.display());
            match crate::updater::read_update_state(&state_path)? {
                Some(state) => {
                    println!("lastRunUnixMs: {}", state.last_run_unix_ms);
                    println!(
                        "lastDetectedInstallationSource: {}",
                        state.last_detected_installation_source.as_str()
                    );
                    println!("checkStatus: {}", state.check_status);
                    println!(
                        "lastError: {}",
                        state.last_error.as_deref().unwrap_or("(none)")
                    );
                    println!(
                        "autoInstallObserved: {}",
                        yes_no(state.auto_install_observed)
                    );
                    println!(
                        "installationAttempted: {}",
                        yes_no(state.installation_attempted)
                    );
                }
                None => println!("persistedState: (none)"),
            }
            println!("currentInstallationSource: {}", installation.source_label());
            println!("currentInstallationEvidence: {}", installation.evidence);
            println!("autoInstallPreference: {}", yes_no(prefs.auto_install));
            let mut runner = RealServiceCommandRunner;
            let fs = RealServiceFileSystem;
            let registration = service_registration_status(
                ServicePlatform::current(),
                &config_home,
                &fs,
                &mut runner,
            );
            println!("serviceRegistration: {}", registration.label());
            println!("serviceRegistrationDetail: {}", registration.detail());
            println!("serviceInstallCommand: rebon update service install");
            println!("packageInstallation: inactive");
            Ok(())
        }
        UpdateServiceCommand::RunOnce => {
            let _lock = match UpdateLock::acquire(&lock_path) {
                Ok(lock) => lock,
                Err(UpdateLockError::Contended { path }) => {
                    println!("updateServiceRunOnce: skipped");
                    println!("reason: lock contended at {}", path.display());
                    return Ok(());
                }
                Err(err) => return Err(err.into()),
            };
            let prefs = rebon_config::load_update_preferences()?;
            let installation = detect_current_installation();
            let state = PersistedUpdateState::dry_run(
                now_unix_ms(),
                installation.installation_type,
                "skipped",
                Some("dry run: network update checks and package installation are not active in service run-once".to_string()),
                prefs.auto_install,
            );
            crate::updater::write_update_state(&state_path, &state)?;
            println!("updateServiceRunOnce: dry-run complete");
            println!("checkStatus: skipped");
            println!("installationAttempted: no");
            println!("statePath: {}", state_path.display());
            println!("installationSource: {}", installation.source_label());
            println!("autoInstallPreference: {}", yes_no(prefs.auto_install));
            println!("packageInstallation: inactive");
            Ok(())
        }
        UpdateServiceCommand::Install => {
            let exe = current_exe_for_service_registration()?;
            let spec = service_install_spec(ServicePlatform::current(), &exe, &config_home);
            let mut fs = RealServiceFileSystem;
            let mut runner = RealServiceCommandRunner;
            install_service(&spec, &mut fs, &mut runner)?;
            println!("updateServiceInstall: registered");
            println!("{}", spec.preview);
            for command in spec.commands {
                println!("commandExecuted: {}", command.preview);
            }
            println!("scope: per-user only; no admin service or privileged location was used");
            println!("packageInstallation: inactive");
            Ok(())
        }
        UpdateServiceCommand::Uninstall => {
            let spec = service_uninstall_spec(ServicePlatform::current(), &config_home);
            let mut fs = RealServiceFileSystem;
            let mut runner = RealServiceCommandRunner;
            uninstall_service(&spec, &mut fs, &mut runner)?;
            println!("updateServiceUninstall: unregistered");
            println!("{}", spec.preview);
            for command in spec.commands {
                println!("commandExecuted: {}", command.preview);
            }
            println!("scope: removed only Rebon per-user registration/artifacts");
            println!("packageInstallation: inactive");
            Ok(())
        }
    }
}

// ------------------------------------------------- background supervisor

/// The supervisor scheduler's registration, as `rebon bg status` prints it.
pub fn supervisor_service_status_lines() -> Vec<String> {
    let config_home = rebon_config::config_home_dir();
    let fs = RealServiceFileSystem;
    let mut runner = RealServiceCommandRunner;
    let registration = supervisor_service_registration_status(
        ServicePlatform::current(),
        &config_home,
        &fs,
        &mut runner,
    );
    vec![
        format!("serviceRegistration: {}", registration.label()),
        format!("serviceRegistrationDetail: {}", registration.detail()),
        "serviceInstallCommand: rebon agents service install".to_string(),
    ]
}

/// Register the per-user supervisor scheduler, and describe what was done.
pub fn install_supervisor_service() -> anyhow::Result<Vec<String>> {
    let config_home = rebon_config::config_home_dir();
    let exe = current_exe_for_service_registration()?;
    let spec = supervisor_service_install_spec(ServicePlatform::current(), &exe, &config_home);
    let mut fs = RealServiceFileSystem;
    let mut runner = RealServiceCommandRunner;
    install_service(&spec, &mut fs, &mut runner)?;
    let mut lines = vec![
        "supervisorServiceInstall: registered".to_string(),
        spec.preview.clone(),
    ];
    lines.extend(
        spec.commands
            .iter()
            .map(|command| format!("commandExecuted: {}", command.preview)),
    );
    lines
        .push("scope: per-user only; no admin service or privileged location was used".to_string());
    lines.push(format!(
        "uninstallCommand: {}",
        supervisor_service_uninstall_command()
    ));
    Ok(lines)
}

/// Remove the per-user supervisor scheduler registration.
pub fn uninstall_supervisor_service() -> anyhow::Result<Vec<String>> {
    let config_home = rebon_config::config_home_dir();
    let spec = supervisor_service_uninstall_spec(ServicePlatform::current(), &config_home);
    let mut fs = RealServiceFileSystem;
    let mut runner = RealServiceCommandRunner;
    uninstall_service(&spec, &mut fs, &mut runner)?;
    let mut lines = vec![
        "supervisorServiceUninstall: unregistered".to_string(),
        spec.preview.clone(),
    ];
    lines.extend(
        spec.commands
            .iter()
            .map(|command| format!("commandExecuted: {}", command.preview)),
    );
    lines.push("scope: removed only Rebon per-user supervisor registration/artifacts".to_string());
    Ok(lines)
}

/// Migrate a legacy Windows scheduler launcher, if one is registered.
///
/// `Ok(true)` means a migration happened and is worth a log line; the caller
/// owns the supervisor's log, so the sentence it writes stays with it.
pub fn migrate_legacy_supervisor_service(
    rebon_exe: &Path,
    store_root: &Path,
) -> Result<bool, crate::updater::ServiceRegistrationError> {
    let mut fs = RealServiceFileSystem;
    let mut runner = RealServiceCommandRunner;
    crate::updater::migrate_legacy_supervisor_service(
        ServicePlatform::current(),
        rebon_exe,
        store_root,
        &mut fs,
        &mut runner,
    )
}

pub fn supervisor_service_uninstall_command() -> &'static str {
    "rebon bg uninstall"
}

pub fn supervisor_service_install_hint_message() -> &'static str {
    "rebon: background supervisor scheduler is not installed. Run `rebon bg install` to keep queued jobs moving after login or on the next scheduler tick."
}

pub fn should_emit_supervisor_service_install_hint(
    registration: &ServiceRegistrationStatus,
) -> bool {
    matches!(registration, ServiceRegistrationStatus::NotRegistered(_))
}

/// The hint a command that just queued work prints when nothing will pick it
/// up after logout. `None` when the scheduler is registered, or when the
/// platform cannot say.
pub fn supervisor_service_install_hint() -> Option<String> {
    let config_home = rebon_config::config_home_dir();
    let fs = RealServiceFileSystem;
    let mut runner = RealServiceCommandRunner;
    let registration = supervisor_service_registration_status(
        ServicePlatform::current(),
        &config_home,
        &fs,
        &mut runner,
    );
    should_emit_supervisor_service_install_hint(&registration)
        .then(|| supervisor_service_install_hint_message().to_string())
}

fn current_exe_for_service_registration() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    std::env::current_exe().context("failed to locate the rebon executable to register the service")
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_service_hint_shows_for_each_related_command_when_registration_is_missing() {
        let missing = ServiceRegistrationStatus::NotRegistered("missing".into());
        let registered = ServiceRegistrationStatus::Registered("registered".into());
        let unknown = ServiceRegistrationStatus::Unknown("unknown".into());

        assert!(should_emit_supervisor_service_install_hint(&missing));
        assert!(should_emit_supervisor_service_install_hint(&missing));
        assert!(!should_emit_supervisor_service_install_hint(&registered));
        assert!(!should_emit_supervisor_service_install_hint(&unknown));
        assert!(supervisor_service_install_hint_message().contains("rebon bg install"));
        assert_eq!(supervisor_service_uninstall_command(), "rebon bg uninstall");
    }
}
