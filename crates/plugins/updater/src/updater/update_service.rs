use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SERVICE_TASK_NAME: &str = "RebonAutoUpdate";
const MACOS_LABEL: &str = "com.rebon.update";
const LINUX_SERVICE_NAME: &str = "rebon-update.service";
const LINUX_TIMER_NAME: &str = "rebon-update.timer";
pub const SUPERVISOR_SERVICE_TASK_NAME: &str = "RebonBackgroundSupervisor";
const SUPERVISOR_MACOS_LABEL: &str = "com.rebon.background-supervisor";
const SUPERVISOR_LINUX_SERVICE_NAME: &str = "rebon-session-host-supervisor.service";
const SUPERVISOR_LINUX_TIMER_NAME: &str = "rebon-session-host-supervisor.timer";
const SUPERVISOR_WINDOWS_LAUNCHER_NAME: &str = "background-supervisor.vbs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicePlatform {
    Windows,
    Macos,
    Linux,
}

impl ServicePlatform {
    pub fn current() -> Self {
        #[cfg(windows)]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            Self::Linux
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub preview: String,
}

impl ServiceCommandSpec {
    fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        let program = program.into();
        let preview = std::iter::once(program.clone())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            program,
            args,
            preview,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInstallSpec {
    pub platform: ServicePlatform,
    pub windows_task_name: String,
    pub macos_label: String,
    pub linux_service_name: String,
    pub linux_timer_name: String,
    pub service_description: String,
    pub timer_description: String,
    pub interval_seconds: u64,
    pub run_at_load: bool,
    pub linux_on_boot_sec: String,
    pub registration_path: Option<PathBuf>,
    pub artifact_paths: Vec<PathBuf>,
    pub target_program: String,
    pub target_args: Vec<String>,
    pub commands: Vec<ServiceCommandSpec>,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ServiceCommandOutput {
    pub fn success() -> Self {
        Self {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn failed(status: i32, stderr: impl Into<String>) -> Self {
        Self {
            status,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == 0
    }
}

#[derive(Debug)]
pub enum ServiceRegistrationError {
    Io(io::Error),
    CommandFailed {
        command: ServiceCommandSpec,
        status: i32,
        stdout: String,
        stderr: String,
    },
}

impl std::fmt::Display for ServiceRegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::CommandFailed {
                command,
                status,
                stdout,
                stderr,
            } => write!(
                f,
                "service command failed (status {status}): {} stdout={} stderr={}",
                command.preview,
                stdout.trim(),
                stderr.trim()
            ),
        }
    }
}

impl std::error::Error for ServiceRegistrationError {}

impl From<io::Error> for ServiceRegistrationError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type ServiceRegistrationResult<T> = Result<T, ServiceRegistrationError>;

pub trait ServiceCommandRunner {
    fn run(
        &mut self,
        command: &ServiceCommandSpec,
    ) -> ServiceRegistrationResult<ServiceCommandOutput>;
}

pub trait ServiceFileSystem {
    fn create_dir_all(&mut self, path: &Path) -> ServiceRegistrationResult<()>;
    fn write(&mut self, path: &Path, contents: &str) -> ServiceRegistrationResult<()>;
    fn remove_file_if_exists(&mut self, path: &Path) -> ServiceRegistrationResult<()>;
    fn exists(&self, path: &Path) -> bool;
}

#[derive(Debug, Default)]
pub struct RealServiceCommandRunner;

impl ServiceCommandRunner for RealServiceCommandRunner {
    fn run(
        &mut self,
        command: &ServiceCommandSpec,
    ) -> ServiceRegistrationResult<ServiceCommandOutput> {
        let output = Command::new(&command.program)
            .args(&command.args)
            .output()?;
        Ok(ServiceCommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[derive(Debug, Default)]
pub struct RealServiceFileSystem;

impl ServiceFileSystem for RealServiceFileSystem {
    fn create_dir_all(&mut self, path: &Path) -> ServiceRegistrationResult<()> {
        fs::create_dir_all(path).map_err(Into::into)
    }

    fn write(&mut self, path: &Path, contents: &str) -> ServiceRegistrationResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents).map_err(Into::into)
    }

    fn remove_file_if_exists(&mut self, path: &Path) -> ServiceRegistrationResult<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceRegistrationStatus {
    Registered(String),
    NotRegistered(String),
    Unknown(String),
}

impl ServiceRegistrationStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Registered(_) => "registered",
            Self::NotRegistered(_) => "not registered",
            Self::Unknown(_) => "unknown",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::Registered(detail) | Self::NotRegistered(detail) | Self::Unknown(detail) => {
                detail
            }
        }
    }
}

pub fn service_install_spec(
    platform: ServicePlatform,
    rebon_exe: &Path,
    config_home: &Path,
) -> ServiceInstallSpec {
    match platform {
        ServicePlatform::Windows => windows_spec(rebon_exe),
        ServicePlatform::Macos => macos_spec(rebon_exe, config_home),
        ServicePlatform::Linux => linux_spec(rebon_exe, config_home),
    }
}

pub fn supervisor_service_install_spec(
    platform: ServicePlatform,
    rebon_exe: &Path,
    config_home: &Path,
) -> ServiceInstallSpec {
    match platform {
        ServicePlatform::Windows => supervisor_windows_spec(rebon_exe, config_home),
        ServicePlatform::Macos => supervisor_macos_spec(rebon_exe, config_home),
        ServicePlatform::Linux => supervisor_linux_spec(rebon_exe, config_home),
    }
}

pub fn service_uninstall_spec(platform: ServicePlatform, config_home: &Path) -> ServiceInstallSpec {
    match platform {
        ServicePlatform::Windows => ServiceInstallSpec {
            platform,
            windows_task_name: SERVICE_TASK_NAME.into(),
            macos_label: MACOS_LABEL.into(),
            linux_service_name: LINUX_SERVICE_NAME.into(),
            linux_timer_name: LINUX_TIMER_NAME.into(),
            service_description: "Rebon auto-update dry-run runner".into(),
            timer_description: "Run Rebon auto-update dry-run periodically".into(),
            interval_seconds: 3600,
            run_at_load: false,
            linux_on_boot_sec: "5m".into(),
            registration_path: None,
            artifact_paths: Vec::new(),
            target_program: String::new(),
            target_args: Vec::new(),
            commands: vec![ServiceCommandSpec::new(
                "schtasks.exe",
                vec![
                    "/Delete".into(),
                    "/TN".into(),
                    SERVICE_TASK_NAME.into(),
                    "/F".into(),
                ],
            )],
            preview: "Remove Rebon per-user Scheduled Task registration only.".into(),
        },
        ServicePlatform::Macos => {
            let path = macos_plist_path(config_home, MACOS_LABEL);
            ServiceInstallSpec {
                platform,
                windows_task_name: SERVICE_TASK_NAME.into(),
                macos_label: MACOS_LABEL.into(),
                linux_service_name: LINUX_SERVICE_NAME.into(),
                linux_timer_name: LINUX_TIMER_NAME.into(),
                service_description: "Rebon auto-update dry-run runner".into(),
                timer_description: "Run Rebon auto-update dry-run periodically".into(),
                interval_seconds: 3600,
                run_at_load: false,
                linux_on_boot_sec: "5m".into(),
                registration_path: Some(path.clone()),
                artifact_paths: vec![path.clone()],
                target_program: String::new(),
                target_args: Vec::new(),
                commands: vec![ServiceCommandSpec::new(
                    "launchctl",
                    vec!["unload".into(), "-w".into(), path.display().to_string()],
                )],
                preview: "Remove Rebon per-user LaunchAgent registration only.".into(),
            }
        }
        ServicePlatform::Linux => {
            let paths = linux_artifact_paths(config_home, LINUX_SERVICE_NAME, LINUX_TIMER_NAME);
            ServiceInstallSpec {
                platform,
                windows_task_name: SERVICE_TASK_NAME.into(),
                macos_label: MACOS_LABEL.into(),
                linux_service_name: LINUX_SERVICE_NAME.into(),
                linux_timer_name: LINUX_TIMER_NAME.into(),
                service_description: "Rebon auto-update dry-run runner".into(),
                timer_description: "Run Rebon auto-update dry-run periodically".into(),
                interval_seconds: 3600,
                run_at_load: false,
                linux_on_boot_sec: "5m".into(),
                registration_path: Some(paths.timer.clone()),
                artifact_paths: vec![paths.service.clone(), paths.timer.clone()],
                target_program: String::new(),
                target_args: Vec::new(),
                commands: vec![
                    ServiceCommandSpec::new(
                        "systemctl",
                        vec![
                            "--user".into(),
                            "disable".into(),
                            "--now".into(),
                            LINUX_TIMER_NAME.into(),
                        ],
                    ),
                    ServiceCommandSpec::new(
                        "systemctl",
                        vec!["--user".into(), "daemon-reload".into()],
                    ),
                ],
                preview: "Remove Rebon systemd user timer registration only.".into(),
            }
        }
    }
}

pub fn supervisor_service_uninstall_spec(
    platform: ServicePlatform,
    config_home: &Path,
) -> ServiceInstallSpec {
    match platform {
        ServicePlatform::Windows => {
            let path = windows_supervisor_launcher_path(config_home);
            ServiceInstallSpec {
                platform,
                windows_task_name: SUPERVISOR_SERVICE_TASK_NAME.into(),
                macos_label: SUPERVISOR_MACOS_LABEL.into(),
                linux_service_name: SUPERVISOR_LINUX_SERVICE_NAME.into(),
                linux_timer_name: SUPERVISOR_LINUX_TIMER_NAME.into(),
                service_description: "Rebon background supervisor".into(),
                timer_description: "Run Rebon background supervisor periodically".into(),
                interval_seconds: 300,
                run_at_load: true,
                linux_on_boot_sec: "1m".into(),
                registration_path: Some(path.clone()),
                artifact_paths: vec![path],
                target_program: String::new(),
                target_args: Vec::new(),
                commands: vec![ServiceCommandSpec::new(
                    "schtasks.exe",
                    vec![
                        "/Delete".into(),
                        "/TN".into(),
                        SUPERVISOR_SERVICE_TASK_NAME.into(),
                        "/F".into(),
                    ],
                )],
                preview: "Remove Rebon background supervisor Scheduled Task registration and hidden launcher."
                    .into(),
            }
        }
        ServicePlatform::Macos => {
            let path = macos_plist_path(config_home, SUPERVISOR_MACOS_LABEL);
            ServiceInstallSpec {
                platform,
                windows_task_name: SUPERVISOR_SERVICE_TASK_NAME.into(),
                macos_label: SUPERVISOR_MACOS_LABEL.into(),
                linux_service_name: SUPERVISOR_LINUX_SERVICE_NAME.into(),
                linux_timer_name: SUPERVISOR_LINUX_TIMER_NAME.into(),
                service_description: "Rebon background supervisor".into(),
                timer_description: "Run Rebon background supervisor periodically".into(),
                interval_seconds: 300,
                run_at_load: true,
                linux_on_boot_sec: "1m".into(),
                registration_path: Some(path.clone()),
                artifact_paths: vec![path.clone()],
                target_program: String::new(),
                target_args: Vec::new(),
                commands: vec![ServiceCommandSpec::new(
                    "launchctl",
                    vec!["unload".into(), "-w".into(), path.display().to_string()],
                )],
                preview: "Remove Rebon background supervisor LaunchAgent registration only.".into(),
            }
        }
        ServicePlatform::Linux => {
            let paths = linux_artifact_paths(
                config_home,
                SUPERVISOR_LINUX_SERVICE_NAME,
                SUPERVISOR_LINUX_TIMER_NAME,
            );
            ServiceInstallSpec {
                platform,
                windows_task_name: SUPERVISOR_SERVICE_TASK_NAME.into(),
                macos_label: SUPERVISOR_MACOS_LABEL.into(),
                linux_service_name: SUPERVISOR_LINUX_SERVICE_NAME.into(),
                linux_timer_name: SUPERVISOR_LINUX_TIMER_NAME.into(),
                service_description: "Rebon background supervisor".into(),
                timer_description: "Run Rebon background supervisor periodically".into(),
                interval_seconds: 300,
                run_at_load: true,
                linux_on_boot_sec: "1m".into(),
                registration_path: Some(paths.timer.clone()),
                artifact_paths: vec![paths.service.clone(), paths.timer.clone()],
                target_program: String::new(),
                target_args: Vec::new(),
                commands: vec![
                    ServiceCommandSpec::new(
                        "systemctl",
                        vec![
                            "--user".into(),
                            "disable".into(),
                            "--now".into(),
                            SUPERVISOR_LINUX_TIMER_NAME.into(),
                        ],
                    ),
                    ServiceCommandSpec::new(
                        "systemctl",
                        vec!["--user".into(), "daemon-reload".into()],
                    ),
                ],
                preview: "Remove Rebon background supervisor systemd user timer registration only."
                    .into(),
            }
        }
    }
}

pub fn migrate_legacy_supervisor_service<R: ServiceCommandRunner, F: ServiceFileSystem>(
    platform: ServicePlatform,
    rebon_exe: &Path,
    config_home: &Path,
    fs: &mut F,
    runner: &mut R,
) -> ServiceRegistrationResult<bool> {
    if platform != ServicePlatform::Windows {
        return Ok(false);
    }
    let query = ServiceCommandSpec::new(
        "schtasks.exe",
        vec![
            "/Query".into(),
            "/TN".into(),
            SUPERVISOR_SERVICE_TASK_NAME.into(),
            "/XML".into(),
        ],
    );
    let output = runner.run(&query)?;
    if !output.is_success() || !windows_task_has_legacy_supervisor_action(&output.stdout) {
        return Ok(false);
    }

    let mut spec = supervisor_windows_spec(rebon_exe, config_home);
    let command = windows_supervisor_task_command(
        spec.registration_path
            .as_deref()
            .expect("Windows supervisor launcher path"),
    );
    spec.commands = vec![ServiceCommandSpec::new(
        "schtasks.exe",
        vec![
            "/Change".into(),
            "/TN".into(),
            SUPERVISOR_SERVICE_TASK_NAME.into(),
            "/TR".into(),
            command,
        ],
    )];
    install_service(&spec, fs, runner)?;
    Ok(true)
}

pub fn install_service<R: ServiceCommandRunner, F: ServiceFileSystem>(
    spec: &ServiceInstallSpec,
    fs: &mut F,
    runner: &mut R,
) -> ServiceRegistrationResult<()> {
    match spec.platform {
        ServicePlatform::Windows => {
            if let Some(launcher_path) = &spec.registration_path {
                let launcher_existed = fs.exists(launcher_path);
                if let Some(parent) = launcher_path.parent() {
                    fs.create_dir_all(parent)?;
                }
                fs.write(launcher_path, &windows_hidden_launcher_contents(spec))?;
                if launcher_existed {
                    run_all(&spec.commands, runner)
                } else {
                    run_all_with_artifact_rollback(&spec.commands, &spec.artifact_paths, fs, runner)
                }
            } else {
                run_all(&spec.commands, runner)
            }
        }
        ServicePlatform::Macos => {
            let plist_path = spec.registration_path.as_ref().expect("macOS plist path");
            if let Some(parent) = plist_path.parent() {
                fs.create_dir_all(parent)?;
            }
            fs.write(plist_path, &macos_plist_contents(spec))?;
            run_all_with_artifact_rollback(&spec.commands, &spec.artifact_paths, fs, runner)
        }
        ServicePlatform::Linux => {
            let paths = linux_paths_from_spec(spec);
            if let Some(parent) = paths.service.parent() {
                fs.create_dir_all(parent)?;
            }
            fs.write(&paths.service, &linux_service_contents(spec))?;
            fs.write(&paths.timer, &linux_timer_contents(spec))?;
            run_all_with_artifact_rollback(&spec.commands, &spec.artifact_paths, fs, runner)
        }
    }
}

pub fn uninstall_service<R: ServiceCommandRunner, F: ServiceFileSystem>(
    spec: &ServiceInstallSpec,
    fs: &mut F,
    runner: &mut R,
) -> ServiceRegistrationResult<()> {
    match spec.platform {
        ServicePlatform::Windows => {
            run_all(&spec.commands, runner)?;
            for path in &spec.artifact_paths {
                fs.remove_file_if_exists(path)?;
            }
            Ok(())
        }
        ServicePlatform::Macos | ServicePlatform::Linux => {
            let command_result = run_all(&spec.commands, runner);
            for path in &spec.artifact_paths {
                fs.remove_file_if_exists(path)?;
            }
            command_result
        }
    }
}

pub fn service_registration_status<R: ServiceCommandRunner, F: ServiceFileSystem>(
    platform: ServicePlatform,
    config_home: &Path,
    fs: &F,
    runner: &mut R,
) -> ServiceRegistrationStatus {
    service_registration_status_for(
        platform,
        config_home,
        fs,
        runner,
        ServiceRegistrationNames {
            windows_task_name: SERVICE_TASK_NAME,
            macos_label: MACOS_LABEL,
            linux_service_name: LINUX_SERVICE_NAME,
            linux_timer_name: LINUX_TIMER_NAME,
        },
    )
}

pub fn supervisor_service_registration_status<R: ServiceCommandRunner, F: ServiceFileSystem>(
    platform: ServicePlatform,
    config_home: &Path,
    fs: &F,
    runner: &mut R,
) -> ServiceRegistrationStatus {
    service_registration_status_for(
        platform,
        config_home,
        fs,
        runner,
        ServiceRegistrationNames {
            windows_task_name: SUPERVISOR_SERVICE_TASK_NAME,
            macos_label: SUPERVISOR_MACOS_LABEL,
            linux_service_name: SUPERVISOR_LINUX_SERVICE_NAME,
            linux_timer_name: SUPERVISOR_LINUX_TIMER_NAME,
        },
    )
}

#[derive(Debug, Clone, Copy)]
struct ServiceRegistrationNames {
    windows_task_name: &'static str,
    macos_label: &'static str,
    linux_service_name: &'static str,
    linux_timer_name: &'static str,
}

fn service_registration_status_for<R: ServiceCommandRunner, F: ServiceFileSystem>(
    platform: ServicePlatform,
    config_home: &Path,
    fs: &F,
    runner: &mut R,
    names: ServiceRegistrationNames,
) -> ServiceRegistrationStatus {
    match platform {
        ServicePlatform::Windows => {
            let command = ServiceCommandSpec::new(
                "schtasks.exe",
                vec![
                    "/Query".into(),
                    "/TN".into(),
                    names.windows_task_name.into(),
                ],
            );
            match runner.run(&command) {
                Ok(output) if output.is_success() => ServiceRegistrationStatus::Registered(
                    format!("Scheduled Task `{}` is queryable", names.windows_task_name),
                ),
                Ok(output) => ServiceRegistrationStatus::NotRegistered(format!(
                    "Scheduled Task `{}` query exited with status {}: {}",
                    names.windows_task_name,
                    output.status,
                    output.stderr.trim()
                )),
                Err(err) => ServiceRegistrationStatus::Unknown(format!(
                    "Scheduled Task status query unavailable: {err}"
                )),
            }
        }
        ServicePlatform::Macos => {
            let path = macos_plist_path(config_home, names.macos_label);
            if fs.exists(&path) {
                ServiceRegistrationStatus::Registered(format!(
                    "LaunchAgent plist exists at {}",
                    path.display()
                ))
            } else {
                ServiceRegistrationStatus::NotRegistered(format!(
                    "LaunchAgent plist not found at {}",
                    path.display()
                ))
            }
        }
        ServicePlatform::Linux => {
            let paths = linux_artifact_paths(
                config_home,
                names.linux_service_name,
                names.linux_timer_name,
            );
            if !fs.exists(&paths.timer) || !fs.exists(&paths.service) {
                return ServiceRegistrationStatus::NotRegistered(format!(
                    "systemd user files not found at {} and {}",
                    paths.service.display(),
                    paths.timer.display()
                ));
            }
            let command = ServiceCommandSpec::new(
                "systemctl",
                vec![
                    "--user".into(),
                    "is-enabled".into(),
                    names.linux_timer_name.into(),
                ],
            );
            match runner.run(&command) {
                Ok(output) if output.is_success() => ServiceRegistrationStatus::Registered(
                    format!("systemd user timer `{}` is enabled", names.linux_timer_name),
                ),
                Ok(output) => ServiceRegistrationStatus::Unknown(format!(
                    "systemd user files exist, but is-enabled exited with status {}: {}",
                    output.status,
                    output.stderr.trim()
                )),
                Err(err) => ServiceRegistrationStatus::Unknown(format!(
                    "systemd user files exist, but status query is unavailable: {err}"
                )),
            }
        }
    }
}

fn run_all<R: ServiceCommandRunner>(
    commands: &[ServiceCommandSpec],
    runner: &mut R,
) -> ServiceRegistrationResult<()> {
    for command in commands {
        let output = runner.run(command)?;
        if !output.is_success() {
            return Err(ServiceRegistrationError::CommandFailed {
                command: command.clone(),
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }
    }
    Ok(())
}

fn run_all_with_artifact_rollback<R: ServiceCommandRunner, F: ServiceFileSystem>(
    commands: &[ServiceCommandSpec],
    artifact_paths: &[PathBuf],
    fs: &mut F,
    runner: &mut R,
) -> ServiceRegistrationResult<()> {
    match run_all(commands, runner) {
        Ok(()) => Ok(()),
        Err(err) => {
            let mut rollback_error = None;
            for path in artifact_paths {
                if let Err(remove_err) = fs.remove_file_if_exists(path) {
                    rollback_error = Some(remove_err);
                    break;
                }
            }
            Err(rollback_error.unwrap_or(err))
        }
    }
}

fn windows_spec(rebon_exe: &Path) -> ServiceInstallSpec {
    let command = format!("\"{}\" update service run-once", rebon_exe.display());
    ServiceInstallSpec {
        platform: ServicePlatform::Windows,
        windows_task_name: SERVICE_TASK_NAME.into(),
        macos_label: MACOS_LABEL.into(),
        linux_service_name: LINUX_SERVICE_NAME.into(),
        linux_timer_name: LINUX_TIMER_NAME.into(),
        service_description: "Rebon auto-update dry-run runner".into(),
        timer_description: "Run Rebon auto-update dry-run periodically".into(),
        interval_seconds: 3600,
        run_at_load: false,
        linux_on_boot_sec: "5m".into(),
        registration_path: None,
        artifact_paths: Vec::new(),
        target_program: rebon_exe.display().to_string(),
        target_args: vec!["update".into(), "service".into(), "run-once".into()],
        commands: vec![ServiceCommandSpec::new(
            "schtasks.exe",
            vec![
                "/Create".into(),
                "/SC".into(),
                "HOURLY".into(),
                "/TN".into(),
                SERVICE_TASK_NAME.into(),
                "/TR".into(),
                command,
                "/F".into(),
            ],
        )],
        preview: "Per-user Windows Scheduled Task for `rebon update service run-once`; package installation remains inactive.".into(),
    }
}

fn macos_spec(rebon_exe: &Path, config_home: &Path) -> ServiceInstallSpec {
    let path = macos_plist_path(config_home, MACOS_LABEL);
    ServiceInstallSpec {
        platform: ServicePlatform::Macos,
        windows_task_name: SERVICE_TASK_NAME.into(),
        macos_label: MACOS_LABEL.into(),
        linux_service_name: LINUX_SERVICE_NAME.into(),
        linux_timer_name: LINUX_TIMER_NAME.into(),
        service_description: "Rebon auto-update dry-run runner".into(),
        timer_description: "Run Rebon auto-update dry-run periodically".into(),
        interval_seconds: 3600,
        run_at_load: false,
        linux_on_boot_sec: "5m".into(),
        registration_path: Some(path.clone()),
        artifact_paths: vec![path.clone()],
        target_program: rebon_exe.display().to_string(),
        target_args: vec!["update".into(), "service".into(), "run-once".into()],
        commands: vec![ServiceCommandSpec::new(
            "launchctl",
            vec!["load".into(), "-w".into(), path.display().to_string()],
        )],
        preview: format!(
            "Per-user LaunchAgent at {} runs: {} update service run-once; package installation remains inactive.",
            path.display(),
            rebon_exe.display()
        ),
    }
}

fn linux_spec(rebon_exe: &Path, config_home: &Path) -> ServiceInstallSpec {
    let paths = linux_artifact_paths(config_home, LINUX_SERVICE_NAME, LINUX_TIMER_NAME);
    ServiceInstallSpec {
        platform: ServicePlatform::Linux,
        windows_task_name: SERVICE_TASK_NAME.into(),
        macos_label: MACOS_LABEL.into(),
        linux_service_name: LINUX_SERVICE_NAME.into(),
        linux_timer_name: LINUX_TIMER_NAME.into(),
        service_description: "Rebon auto-update dry-run runner".into(),
        timer_description: "Run Rebon auto-update dry-run periodically".into(),
        interval_seconds: 3600,
        run_at_load: false,
        linux_on_boot_sec: "5m".into(),
        registration_path: Some(paths.timer.clone()),
        artifact_paths: vec![paths.service.clone(), paths.timer.clone()],
        target_program: rebon_exe.display().to_string(),
        target_args: vec!["update".into(), "service".into(), "run-once".into()],
        commands: vec![
            ServiceCommandSpec::new("systemctl", vec!["--user".into(), "daemon-reload".into()]),
            ServiceCommandSpec::new(
                "systemctl",
                vec!["--user".into(), "enable".into(), "--now".into(), LINUX_TIMER_NAME.into()],
            ),
        ],
        preview: format!(
            "systemd user timer at {} runs: {} update service run-once; package installation remains inactive.",
            paths.timer.display(),
            rebon_exe.display()
        ),
    }
}

fn supervisor_windows_spec(rebon_exe: &Path, config_home: &Path) -> ServiceInstallSpec {
    let launcher_path = windows_supervisor_launcher_path(config_home);
    let command = windows_supervisor_task_command(&launcher_path);
    ServiceInstallSpec {
        platform: ServicePlatform::Windows,
        windows_task_name: SUPERVISOR_SERVICE_TASK_NAME.into(),
        macos_label: SUPERVISOR_MACOS_LABEL.into(),
        linux_service_name: SUPERVISOR_LINUX_SERVICE_NAME.into(),
        linux_timer_name: SUPERVISOR_LINUX_TIMER_NAME.into(),
        service_description: "Rebon background supervisor".into(),
        timer_description: "Run Rebon background supervisor periodically".into(),
        interval_seconds: 300,
        run_at_load: true,
        linux_on_boot_sec: "1m".into(),
        registration_path: Some(launcher_path.clone()),
        artifact_paths: vec![launcher_path],
        target_program: rebon_exe.display().to_string(),
        target_args: vec!["__background-supervisor".into()],
        commands: vec![ServiceCommandSpec::new(
            "schtasks.exe",
            vec![
                "/Create".into(),
                "/SC".into(),
                "MINUTE".into(),
                "/MO".into(),
                "5".into(),
                "/TN".into(),
                SUPERVISOR_SERVICE_TASK_NAME.into(),
                "/TR".into(),
                command,
                "/F".into(),
            ],
        )],
        preview: "Per-user Windows Scheduled Task for the hidden `rebon __background-supervisor` launcher."
            .into(),
    }
}

fn supervisor_macos_spec(rebon_exe: &Path, config_home: &Path) -> ServiceInstallSpec {
    let path = macos_plist_path(config_home, SUPERVISOR_MACOS_LABEL);
    ServiceInstallSpec {
        platform: ServicePlatform::Macos,
        windows_task_name: SUPERVISOR_SERVICE_TASK_NAME.into(),
        macos_label: SUPERVISOR_MACOS_LABEL.into(),
        linux_service_name: SUPERVISOR_LINUX_SERVICE_NAME.into(),
        linux_timer_name: SUPERVISOR_LINUX_TIMER_NAME.into(),
        service_description: "Rebon background supervisor".into(),
        timer_description: "Run Rebon background supervisor periodically".into(),
        interval_seconds: 300,
        run_at_load: true,
        linux_on_boot_sec: "1m".into(),
        registration_path: Some(path.clone()),
        artifact_paths: vec![path.clone()],
        target_program: rebon_exe.display().to_string(),
        target_args: vec!["__background-supervisor".into()],
        commands: vec![ServiceCommandSpec::new(
            "launchctl",
            vec!["load".into(), "-w".into(), path.display().to_string()],
        )],
        preview: format!(
            "Per-user LaunchAgent at {} runs: {} __background-supervisor.",
            path.display(),
            rebon_exe.display()
        ),
    }
}

fn supervisor_linux_spec(rebon_exe: &Path, config_home: &Path) -> ServiceInstallSpec {
    let paths = linux_artifact_paths(
        config_home,
        SUPERVISOR_LINUX_SERVICE_NAME,
        SUPERVISOR_LINUX_TIMER_NAME,
    );
    ServiceInstallSpec {
        platform: ServicePlatform::Linux,
        windows_task_name: SUPERVISOR_SERVICE_TASK_NAME.into(),
        macos_label: SUPERVISOR_MACOS_LABEL.into(),
        linux_service_name: SUPERVISOR_LINUX_SERVICE_NAME.into(),
        linux_timer_name: SUPERVISOR_LINUX_TIMER_NAME.into(),
        service_description: "Rebon background supervisor".into(),
        timer_description: "Run Rebon background supervisor periodically".into(),
        interval_seconds: 300,
        run_at_load: true,
        linux_on_boot_sec: "1m".into(),
        registration_path: Some(paths.timer.clone()),
        artifact_paths: vec![paths.service.clone(), paths.timer.clone()],
        target_program: rebon_exe.display().to_string(),
        target_args: vec!["__background-supervisor".into()],
        commands: vec![
            ServiceCommandSpec::new("systemctl", vec!["--user".into(), "daemon-reload".into()]),
            ServiceCommandSpec::new(
                "systemctl",
                vec![
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    SUPERVISOR_LINUX_TIMER_NAME.into(),
                ],
            ),
        ],
        preview: format!(
            "systemd user timer at {} runs: {} __background-supervisor.",
            paths.timer.display(),
            rebon_exe.display()
        ),
    }
}

fn windows_supervisor_launcher_path(config_home: &Path) -> PathBuf {
    config_home.join(SUPERVISOR_WINDOWS_LAUNCHER_NAME)
}

fn windows_task_has_legacy_supervisor_action(xml: &str) -> bool {
    let Some(actions) = xml_element_contents(xml, "Actions") else {
        return false;
    };
    let Some(exec) = xml_element_contents(actions, "Exec") else {
        return false;
    };
    exec.contains("__background-supervisor") && !exec.contains(SUPERVISOR_WINDOWS_LAUNCHER_NAME)
}

fn xml_element_contents<'a>(xml: &'a str, element: &str) -> Option<&'a str> {
    let opening = format!("<{element}");
    let opening_start = xml.find(&opening)?;
    let contents_start = opening_start + xml[opening_start..].find('>')? + 1;
    let closing = format!("</{element}>");
    let contents_end = contents_start + xml[contents_start..].find(&closing)?;
    Some(&xml[contents_start..contents_end])
}

fn windows_supervisor_task_command(launcher_path: &Path) -> String {
    format!(
        "wscript.exe //B //NoLogo {}",
        windows_command_line_arg(&launcher_path.display().to_string())
    )
}

fn windows_hidden_launcher_contents(spec: &ServiceInstallSpec) -> String {
    let command = std::iter::once(spec.target_program.as_str())
        .chain(spec.target_args.iter().map(String::as_str))
        .map(windows_command_line_arg)
        .collect::<Vec<_>>()
        .join(" ");
    let command_expression = command
        .encode_utf16()
        .map(|unit| format!("ChrW({unit})"))
        .collect::<Vec<_>>()
        .join(" & ");
    format!(
        "Option Explicit\r\nDim shell, command, exitCode\r\nSet shell = CreateObject(\"WScript.Shell\")\r\ncommand = {command_expression}\r\nexitCode = shell.Run(command, 0, True)\r\nWScript.Quit exitCode\r\n"
    )
}

fn windows_command_line_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes * 2 + 1 {
                    quoted.push('\\');
                }
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    quoted.push('\\');
                }
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    for _ in 0..backslashes * 2 {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

fn macos_plist_path(config_home: &Path, label: &str) -> PathBuf {
    home_from_config(config_home)
        .map(|home| home.join("Library").join("LaunchAgents"))
        .unwrap_or_else(|| config_home.join("launchagents"))
        .join(format!("{label}.plist"))
}

#[derive(Debug, Clone)]
struct LinuxPaths {
    service: PathBuf,
    timer: PathBuf,
}

fn linux_artifact_paths(config_home: &Path, service_name: &str, timer_name: &str) -> LinuxPaths {
    let base = home_from_config(config_home)
        .map(|home| home.join(".config").join("systemd").join("user"))
        .unwrap_or_else(|| config_home.join("systemd-user"));
    LinuxPaths {
        service: base.join(service_name),
        timer: base.join(timer_name),
    }
}

fn linux_paths_from_spec(spec: &ServiceInstallSpec) -> LinuxPaths {
    let service = spec
        .artifact_paths
        .iter()
        .find(|path| {
            path.file_name().and_then(|name| name.to_str())
                == Some(spec.linux_service_name.as_str())
        })
        .cloned()
        .expect("linux service path");
    let timer = spec
        .artifact_paths
        .iter()
        .find(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some(spec.linux_timer_name.as_str())
        })
        .cloned()
        .expect("linux timer path");
    LinuxPaths { service, timer }
}

fn home_from_config(config_home: &Path) -> Option<PathBuf> {
    (config_home.file_name().and_then(|name| name.to_str()) == Some(".rebon"))
        .then(|| config_home.parent().map(Path::to_path_buf))
        .flatten()
}

fn command_program_and_args(spec: &ServiceInstallSpec) -> (&str, &[String]) {
    (&spec.target_program, &spec.target_args)
}

fn macos_plist_contents(spec: &ServiceInstallSpec) -> String {
    let (program, args) = command_program_and_args(spec);
    let args_xml = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .map(|arg| format!("        <string>{}</string>", xml_escape(&arg)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
{}
    </array>
    <key>StartInterval</key>
    <integer>{}</integer>
{}
</dict>
</plist>
"#,
        spec.macos_label,
        args_xml,
        spec.interval_seconds,
        if spec.run_at_load {
            "    <key>RunAtLoad</key>\n    <true/>"
        } else {
            ""
        }
    )
}

fn linux_service_contents(spec: &ServiceInstallSpec) -> String {
    let command = std::iter::once(spec.target_program.as_str())
        .chain(spec.target_args.iter().map(String::as_str))
        .map(systemd_exec_arg_escape)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\nDescription={}\n\n[Service]\nType=oneshot\nExecStart={}\n",
        spec.service_description, command
    )
}

fn systemd_exec_arg_escape(arg: &str) -> String {
    let escaped_percent = arg.replace('%', "%%");
    if escaped_percent.is_empty()
        || escaped_percent
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\' | '\'' | ';' | '#'))
    {
        let mut escaped = String::with_capacity(escaped_percent.len() + 2);
        escaped.push('"');
        for ch in escaped_percent.chars() {
            match ch {
                '"' | '\\' => {
                    escaped.push('\\');
                    escaped.push(ch);
                }
                _ => escaped.push(ch),
            }
        }
        escaped.push('"');
        escaped
    } else {
        escaped_percent
    }
}

fn linux_timer_contents(spec: &ServiceInstallSpec) -> String {
    format!(
        "[Unit]\nDescription={}\n\n[Timer]\nOnBootSec={}\nOnUnitActiveSec={}s\nPersistent=true\nUnit={}\n\n[Install]\nWantedBy=timers.target\n",
        spec.timer_description,
        spec.linux_on_boot_sec,
        spec.interval_seconds,
        spec.linux_service_name
    )
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};

    #[derive(Default)]
    struct FakeRunner {
        calls: Vec<ServiceCommandSpec>,
        failures: HashMap<String, ServiceCommandOutput>,
        responses: VecDeque<ServiceCommandOutput>,
    }

    impl ServiceCommandRunner for FakeRunner {
        fn run(
            &mut self,
            command: &ServiceCommandSpec,
        ) -> ServiceRegistrationResult<ServiceCommandOutput> {
            self.calls.push(command.clone());
            if let Some(output) = self.responses.pop_front() {
                return Ok(output);
            }
            Ok(self
                .failures
                .remove(&command.program)
                .unwrap_or_else(ServiceCommandOutput::success))
        }
    }

    #[derive(Default)]
    struct FakeFs {
        dirs: Vec<PathBuf>,
        files: HashMap<PathBuf, String>,
        removed: Vec<PathBuf>,
        present: HashSet<PathBuf>,
    }

    impl ServiceFileSystem for FakeFs {
        fn create_dir_all(&mut self, path: &Path) -> ServiceRegistrationResult<()> {
            self.dirs.push(path.to_path_buf());
            Ok(())
        }

        fn write(&mut self, path: &Path, contents: &str) -> ServiceRegistrationResult<()> {
            self.present.insert(path.to_path_buf());
            self.files.insert(path.to_path_buf(), contents.to_string());
            Ok(())
        }

        fn remove_file_if_exists(&mut self, path: &Path) -> ServiceRegistrationResult<()> {
            self.present.remove(path);
            self.removed.push(path.to_path_buf());
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            self.present.contains(path)
        }
    }

    #[test]
    fn windows_scheduled_task_spec_registers_current_exe_run_once() {
        let spec = service_install_spec(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
        );
        assert_eq!(spec.commands[0].program, "schtasks.exe");
        assert!(spec.commands[0].args.contains(&"/Create".to_string()));
        assert!(spec.commands[0]
            .args
            .contains(&SERVICE_TASK_NAME.to_string()));
        assert!(spec.commands[0]
            .args
            .iter()
            .any(|arg| arg.contains("rebon.exe") && arg.contains("update service run-once")));
        assert!(!spec.preview.contains("preview only"));
    }

    #[test]
    fn install_windows_runs_fake_schtasks_only() {
        let spec = service_install_spec(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
        );
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        install_service(&spec, &mut fs, &mut runner).unwrap();
        assert_eq!(runner.calls.len(), 1);
        assert_eq!(runner.calls[0].program, "schtasks.exe");
        assert!(fs.files.is_empty());
    }

    #[test]
    fn macos_launchagent_spec_uses_user_launchagents() {
        let spec = service_install_spec(
            ServicePlatform::Macos,
            Path::new("/Applications/Rebon/rebon"),
            Path::new("/Users/me/.rebon"),
        );
        assert_eq!(spec.commands[0].program, "launchctl");
        assert_eq!(
            spec.registration_path,
            Some(PathBuf::from(
                "/Users/me/Library/LaunchAgents/com.rebon.update.plist"
            ))
        );
    }

    #[test]
    fn install_macos_writes_plist_and_loads_with_fake_runner() {
        let spec = service_install_spec(
            ServicePlatform::Macos,
            Path::new("/Applications/Rebon/rebon"),
            Path::new("/Users/me/.rebon"),
        );
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        install_service(&spec, &mut fs, &mut runner).unwrap();
        let plist = spec.registration_path.as_ref().unwrap();
        let contents = fs.files.get(plist).unwrap();
        assert!(contents.contains("<string>com.rebon.update</string>"));
        assert!(contents.contains("update"));
        assert!(contents.contains("run-once"));
        assert_eq!(runner.calls[0].program, "launchctl");
    }

    #[test]
    fn linux_systemd_timer_spec_uses_user_config_scope() {
        let spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/usr/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );
        assert_eq!(spec.commands[0].program, "systemctl");
        assert!(spec.commands[0].args.contains(&"--user".to_string()));
        assert_eq!(
            spec.registration_path,
            Some(PathBuf::from(
                "/home/me/.config/systemd/user/rebon-update.timer"
            ))
        );
        assert!(spec.artifact_paths.contains(&PathBuf::from(
            "/home/me/.config/systemd/user/rebon-update.service"
        )));
    }

    #[test]
    fn install_linux_writes_units_and_enables_timer_with_fake_runner() {
        let spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/usr/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        install_service(&spec, &mut fs, &mut runner).unwrap();
        assert!(fs.files.contains_key(&PathBuf::from(
            "/home/me/.config/systemd/user/rebon-update.service"
        )));
        assert!(fs.files.contains_key(&PathBuf::from(
            "/home/me/.config/systemd/user/rebon-update.timer"
        )));
        assert_eq!(runner.calls.len(), 2);
        assert!(runner
            .calls
            .iter()
            .all(|call| call.args.contains(&"--user".to_string())));
    }

    #[test]
    fn supervisor_service_specs_use_background_supervisor_entrypoint() {
        let windows = supervisor_service_install_spec(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
        );
        assert_eq!(windows.windows_task_name, SUPERVISOR_SERVICE_TASK_NAME);
        assert_eq!(windows.target_args, vec!["__background-supervisor"]);
        assert_eq!(
            windows.registration_path,
            Some(PathBuf::from(
                "C:/Users/me/.rebon/background-supervisor.vbs"
            ))
        );
        assert!(windows.commands[0]
            .args
            .contains(&SUPERVISOR_SERVICE_TASK_NAME.to_string()));
        assert!(windows.commands[0]
            .args
            .iter()
            .any(|arg| arg.contains("wscript.exe") && arg.contains("background-supervisor.vbs")));
        assert!(!windows.commands[0]
            .args
            .iter()
            .any(|arg| arg.contains("rebon.exe") || arg.contains("__background-supervisor")));

        let macos = supervisor_service_install_spec(
            ServicePlatform::Macos,
            Path::new("/Applications/Rebon/rebon"),
            Path::new("/Users/me/.rebon"),
        );
        assert_eq!(
            macos.registration_path,
            Some(PathBuf::from(
                "/Users/me/Library/LaunchAgents/com.rebon.background-supervisor.plist"
            ))
        );
        let plist = macos_plist_contents(&macos);
        assert!(plist.contains("<string>com.rebon.background-supervisor</string>"));
        assert!(plist.contains("__background-supervisor"));
        assert!(plist.contains("<key>RunAtLoad</key>"));

        let linux = supervisor_service_install_spec(
            ServicePlatform::Linux,
            Path::new("/usr/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );
        assert_eq!(
            linux.registration_path,
            Some(PathBuf::from(
                "/home/me/.config/systemd/user/rebon-session-host-supervisor.timer"
            ))
        );
        assert!(linux.artifact_paths.contains(&PathBuf::from(
            "/home/me/.config/systemd/user/rebon-session-host-supervisor.service"
        )));
        assert!(linux_service_contents(&linux)
            .contains("ExecStart=/usr/bin/rebon __background-supervisor\n"));
        assert!(linux_timer_contents(&linux).contains("OnUnitActiveSec=300s"));
        assert!(linux_timer_contents(&linux).contains("Unit=rebon-session-host-supervisor.service"));
    }

    #[test]
    fn install_windows_supervisor_writes_hidden_launcher_and_registers_task() {
        let spec = supervisor_service_install_spec(
            ServicePlatform::Windows,
            Path::new("C:/Program Files/Rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
        );
        let launcher = spec.registration_path.clone().unwrap();
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();

        install_service(&spec, &mut fs, &mut runner).unwrap();

        let contents = fs.files.get(&launcher).unwrap();
        assert!(contents.contains("CreateObject(\"WScript.Shell\")"));
        assert!(contents.contains("shell.Run(command, 0, True)"));
        assert!(contents.contains("WScript.Quit exitCode"));
        assert_eq!(runner.calls.len(), 1);
        assert_eq!(runner.calls[0].program, "schtasks.exe");
    }

    #[test]
    fn windows_supervisor_launcher_is_ascii_for_unicode_executable_paths() {
        let spec = supervisor_service_install_spec(
            ServicePlatform::Windows,
            Path::new("C:/Users/测试😀/Rebon/rebon.exe"),
            Path::new("C:/Users/测试😀/.rebon"),
        );

        let contents = windows_hidden_launcher_contents(&spec);

        assert!(contents.is_ascii());
        assert!(!contents.contains("测试😀"));
        assert!(contents.contains("ChrW(27979)"));
        assert!(contents.contains("ChrW(35797)"));
        assert!(contents.contains("ChrW(55357)"));
        assert!(contents.contains("ChrW(56832)"));
    }

    #[test]
    fn install_windows_supervisor_rolls_back_launcher_when_task_registration_fails() {
        let spec = supervisor_service_install_spec(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
        );
        let launcher = spec.registration_path.clone().unwrap();
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        runner.failures.insert(
            "schtasks.exe".into(),
            ServiceCommandOutput::failed(1, "Access is denied"),
        );

        let err = install_service(&spec, &mut fs, &mut runner).unwrap_err();

        assert!(err.to_string().contains("Access is denied"));
        assert!(!fs.exists(&launcher));
        assert_eq!(fs.removed, vec![launcher]);
    }

    #[test]
    fn uninstall_windows_supervisor_removes_hidden_launcher() {
        let spec = supervisor_service_uninstall_spec(
            ServicePlatform::Windows,
            Path::new("C:/Users/me/.rebon"),
        );
        let launcher = spec.registration_path.clone().unwrap();
        let mut fs = FakeFs::default();
        fs.present.insert(launcher.clone());
        let mut runner = FakeRunner::default();

        uninstall_service(&spec, &mut fs, &mut runner).unwrap();

        assert_eq!(runner.calls[0].program, "schtasks.exe");
        assert_eq!(fs.removed, vec![launcher]);
    }

    #[test]
    fn reinstall_windows_supervisor_preserves_existing_launcher_when_registration_fails() {
        let spec = supervisor_service_install_spec(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
        );
        let launcher = spec.registration_path.clone().unwrap();
        let mut fs = FakeFs::default();
        fs.present.insert(launcher.clone());
        fs.files
            .insert(launcher.clone(), "existing launcher".into());
        let mut runner = FakeRunner::default();
        runner.failures.insert(
            "schtasks.exe".into(),
            ServiceCommandOutput::failed(1, "Access is denied"),
        );

        let err = install_service(&spec, &mut fs, &mut runner).unwrap_err();

        assert!(err.to_string().contains("Access is denied"));
        assert!(fs.exists(&launcher));
        assert!(fs.removed.is_empty());
    }

    #[test]
    fn uninstall_windows_supervisor_preserves_launcher_when_task_deletion_fails() {
        let spec = supervisor_service_uninstall_spec(
            ServicePlatform::Windows,
            Path::new("C:/Users/me/.rebon"),
        );
        let launcher = spec.registration_path.clone().unwrap();
        let mut fs = FakeFs::default();
        fs.present.insert(launcher.clone());
        let mut runner = FakeRunner::default();
        runner.failures.insert(
            "schtasks.exe".into(),
            ServiceCommandOutput::failed(1, "Access is denied"),
        );

        let err = uninstall_service(&spec, &mut fs, &mut runner).unwrap_err();

        assert!(err.to_string().contains("Access is denied"));
        assert!(fs.exists(&launcher));
        assert!(fs.removed.is_empty());
    }

    #[test]
    fn legacy_windows_supervisor_task_migrates_without_recreating_schedule() {
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        runner.responses.push_back(ServiceCommandOutput {
            status: 0,
            stdout: "<Task><Actions><Exec><Command>C:/rebon/rebon.exe</Command><Arguments>__background-supervisor</Arguments></Exec></Actions></Task>"
                .into(),
            stderr: String::new(),
        });
        runner.responses.push_back(ServiceCommandOutput::success());

        let migrated = migrate_legacy_supervisor_service(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
            &mut fs,
            &mut runner,
        )
        .unwrap();

        assert!(migrated);
        assert_eq!(runner.calls.len(), 2);
        assert!(runner.calls[0].args.contains(&"/XML".to_string()));
        assert!(runner.calls[1].args.contains(&"/Change".to_string()));
        assert!(!runner.calls[1].args.contains(&"/Create".to_string()));
        assert!(runner.calls[1]
            .args
            .iter()
            .any(|arg| arg.contains("wscript.exe") && arg.contains("background-supervisor.vbs")));
        assert!(fs.exists(Path::new("C:/Users/me/.rebon/background-supervisor.vbs")));
    }

    #[test]
    fn current_or_missing_windows_supervisor_task_is_not_migrated() {
        let mut fs = FakeFs::default();
        let mut current_runner = FakeRunner::default();
        current_runner.responses.push_back(ServiceCommandOutput {
            status: 0,
            stdout: "<Task><RegistrationInfo><Description>Documentation mentions __background-supervisor</Description></RegistrationInfo><Actions><Exec><Command>wscript.exe</Command><Arguments>//B //NoLogo C:/Users/me/.rebon/background-supervisor.vbs</Arguments></Exec></Actions></Task>"
                .into(),
            stderr: String::new(),
        });

        let current = migrate_legacy_supervisor_service(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
            &mut fs,
            &mut current_runner,
        )
        .unwrap();

        let mut missing_runner = FakeRunner::default();
        missing_runner
            .responses
            .push_back(ServiceCommandOutput::failed(1, "task not found"));
        let missing = migrate_legacy_supervisor_service(
            ServicePlatform::Windows,
            Path::new("C:/rebon/rebon.exe"),
            Path::new("C:/Users/me/.rebon"),
            &mut fs,
            &mut missing_runner,
        )
        .unwrap();

        assert!(!current);
        assert!(!missing);
        assert_eq!(current_runner.calls.len(), 1);
        assert_eq!(missing_runner.calls.len(), 1);
        assert!(fs.files.is_empty());
    }

    #[test]
    fn supervisor_migration_is_a_noop_outside_windows() {
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();

        let migrated = migrate_legacy_supervisor_service(
            ServicePlatform::Linux,
            Path::new("/usr/bin/rebon"),
            Path::new("/home/me/.rebon"),
            &mut fs,
            &mut runner,
        )
        .unwrap();

        assert!(!migrated);
        assert!(runner.calls.is_empty());
        assert!(fs.files.is_empty());
    }

    #[test]
    fn supervisor_service_status_uses_independent_registration_names() {
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        fs.present.insert(PathBuf::from(
            "/home/me/.config/systemd/user/rebon-session-host-supervisor.service",
        ));
        fs.present.insert(PathBuf::from(
            "/home/me/.config/systemd/user/rebon-session-host-supervisor.timer",
        ));

        let linux = supervisor_service_registration_status(
            ServicePlatform::Linux,
            Path::new("/home/me/.rebon"),
            &fs,
            &mut runner,
        );

        assert_eq!(linux.label(), "registered");
        assert_eq!(
            runner.calls[0].args,
            vec![
                "--user".to_string(),
                "is-enabled".to_string(),
                "rebon-session-host-supervisor.timer".to_string()
            ]
        );
    }

    #[test]
    fn linux_systemd_execstart_escapes_executable_path_with_spaces() {
        let spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/opt/Rebon App/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );

        let contents = linux_service_contents(&spec);

        assert!(
            contents.contains("ExecStart=\"/opt/Rebon App/bin/rebon\" update service run-once\n")
        );
        assert!(!contents.contains("ExecStart=/opt/Rebon App/bin/rebon update"));
    }

    #[test]
    fn linux_systemd_execstart_escapes_quotes_and_backslashes() {
        let mut spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/opt/Rebon App/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );
        spec.target_program = "/opt/Rebon \\\"Special\\\"/bin/rebon".to_string();
        spec.target_args = vec![
            "update".into(),
            "service".into(),
            "run-once".into(),
            "arg with space".into(),
        ];

        let contents = linux_service_contents(&spec);

        assert!(contents.contains(
            "ExecStart=\"/opt/Rebon \\\\\\\"Special\\\\\\\"/bin/rebon\" update service run-once \"arg with space\"\n"
        ));
    }

    #[test]
    fn linux_systemd_execstart_escapes_percent_in_executable_path_and_args() {
        let mut spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/opt/Rebon%20/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );
        spec.target_args = vec![
            "update".into(),
            "service".into(),
            "run-once".into(),
            "channel=stable%user".into(),
        ];

        let contents = linux_service_contents(&spec);

        assert!(contents.contains(
            "ExecStart=/opt/Rebon%%20/bin/rebon update service run-once channel=stable%%user\n"
        ));
        assert!(!contents.contains("Rebon%20"));
        assert!(!contents.contains("stable%user"));
    }

    #[test]
    fn linux_systemd_execstart_keeps_run_once_as_separate_arguments() {
        let spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/usr/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );

        let contents = linux_service_contents(&spec);

        assert!(contents.contains("ExecStart=/usr/bin/rebon update service run-once\n"));
        assert!(!contents.contains("update service run-once\""));
        assert!(!contents.contains("\"update service run-once\""));
    }

    #[test]
    fn install_linux_rolls_back_unit_files_when_scheduler_command_fails() {
        let spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/usr/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        runner.failures.insert(
            "systemctl".into(),
            ServiceCommandOutput::failed(1, "Failed to connect to bus"),
        );

        let err = install_service(&spec, &mut fs, &mut runner).unwrap_err();

        assert!(err.to_string().contains("Failed to connect to bus"));
        assert!(!fs.exists(&PathBuf::from(
            "/home/me/.config/systemd/user/rebon-update.service"
        )));
        assert!(!fs.exists(&PathBuf::from(
            "/home/me/.config/systemd/user/rebon-update.timer"
        )));
        assert_eq!(fs.removed.len(), 2);
    }

    #[test]
    fn install_macos_rolls_back_plist_when_scheduler_command_fails() {
        let spec = service_install_spec(
            ServicePlatform::Macos,
            Path::new("/Applications/Rebon/rebon"),
            Path::new("/Users/me/.rebon"),
        );
        let plist = spec.registration_path.clone().unwrap();
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        runner.failures.insert(
            "launchctl".into(),
            ServiceCommandOutput::failed(1, "Load failed"),
        );

        let err = install_service(&spec, &mut fs, &mut runner).unwrap_err();

        assert!(err.to_string().contains("Load failed"));
        assert!(!fs.exists(&plist));
        assert_eq!(fs.removed, vec![plist]);
    }

    #[test]
    fn command_failure_is_reported() {
        let spec = service_install_spec(
            ServicePlatform::Linux,
            Path::new("/usr/bin/rebon"),
            Path::new("/home/me/.rebon"),
        );
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        runner.failures.insert(
            "systemctl".into(),
            ServiceCommandOutput::failed(1, "Failed to connect to bus"),
        );
        let err = install_service(&spec, &mut fs, &mut runner).unwrap_err();
        assert!(err.to_string().contains("Failed to connect to bus"));
    }

    #[test]
    fn uninstall_removes_only_rebon_linux_artifacts() {
        let spec = service_uninstall_spec(ServicePlatform::Linux, Path::new("/home/me/.rebon"));
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        uninstall_service(&spec, &mut fs, &mut runner).unwrap();
        assert_eq!(fs.removed.len(), 2);
        assert!(fs.removed.iter().all(|path| path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rebon-update"))));
    }

    #[test]
    fn status_uses_files_and_fake_queries() {
        let mut fs = FakeFs::default();
        let mut runner = FakeRunner::default();
        let linux = service_registration_status(
            ServicePlatform::Linux,
            Path::new("/home/me/.rebon"),
            &fs,
            &mut runner,
        );
        assert_eq!(linux.label(), "not registered");
        fs.present.insert(PathBuf::from(
            "/home/me/.config/systemd/user/rebon-update.service",
        ));
        fs.present.insert(PathBuf::from(
            "/home/me/.config/systemd/user/rebon-update.timer",
        ));
        let linux = service_registration_status(
            ServicePlatform::Linux,
            Path::new("/home/me/.rebon"),
            &fs,
            &mut runner,
        );
        assert_eq!(linux.label(), "registered");
    }
}
