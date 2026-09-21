//! Turning `settings.json` into a live sandbox.
//!
//! The wrapping half has no opinion about where configuration comes
//! from — [`SandboxRuntime`] takes a compiled
//! [`SessionSandboxConfig`](crate::runtime::SessionSandboxConfig) and wraps
//! commands. This module is the other half: read the settings files,
//! probe the machine, and produce a [`SessionSandbox`] the tool layer
//! turns into its own policy type.
//!
//! It lives here, rather than next to the tools, because three
//! executors are built in three places — the TUI, the ACP server, and
//! the headless harness — and each needs the same answer. A copy per
//! call site is how one of them ends up silently unsandboxed.
//!
//! ## Why a broken setting stops the session
//!
//! Every other settings key here degrades: an unreadable theme falls
//! back to the default, an unparseable model list leaves the model
//! unset. The sandbox block does not, and the asymmetry is the point.
//! A `denyRead` list that failed to parse and quietly became empty
//! looks exactly like a working sandbox right up until something
//! reads the file it was supposed to hide. Refusing to start says so
//! at the only moment the user can still act on it.
//!
//! Two failures are *not* treated that way, because neither is a
//! mistake:
//!
//! * a missing settings file — most projects have none;
//! * a machine with the sandbox turned off — the overwhelmingly
//!   common case, and it costs nothing.

use crate::runtime::confined::{PlatformProbe, SandboxMode};
use crate::runtime::settings::SandboxSettings;
use crate::runtime::support::{
    current_platform, has_backend, probe_support, PathLookup, SupportReport,
};
use crate::runtime::windows::SandboxWinStatus;
use crate::runtime::{SandboxRuntime, SandboxRuntimeInit};
use crate::view::overrides::OverrideMode;
use crate::view::platform::SandboxPlatform;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Everything the session needs to know about its sandbox.
///
/// Deliberately not [`crate::exec::SandboxPolicy`]: that is the value the
/// tool layer holds through its `CommandSandbox` seam, and it carries no
/// support report and no notes because a tool has nothing to do with either.
/// [`SandboxPolicy::from_session`](crate::exec::SandboxPolicy::from_session)
/// converts, and that conversion is the one place the two vocabularies meet.
#[derive(Debug)]
pub struct SessionSandbox {
    /// Whether the sandbox is on *and* enforceable here. False when
    /// settings turned it off, and false when settings turned it on
    /// but the platform has no backend — in the second case `notes`
    /// says so.
    pub enabled: bool,
    pub platform: SandboxPlatform,
    /// `Open` when `allowUnsandboxedCommands` is true.
    pub override_mode: OverrideMode,
    pub excluded_commands: Vec<String>,
    /// `Some` exactly when `enabled` is true.
    pub runtime: Option<Arc<SandboxRuntime>>,
    /// What the machine has, for `/doctor` and `/sandbox`.
    pub support: SupportReport,
    /// Non-fatal notes worth logging — a settings file that could not
    /// be read, a sandbox asked for on a platform that has none.
    pub notes: Vec<String>,
}

/// Why a session refused to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxSetupError {
    /// A settings file carries a `sandbox` block that cannot be read.
    /// The path is named because the three files are merged and the
    /// user otherwise has no way to tell which one is wrong.
    BadSettings { path: String, detail: String },
}

impl std::fmt::Display for SandboxSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxSetupError::BadSettings { path, detail } => write!(f, "{path}: {detail}"),
        }
    }
}

impl std::error::Error for SandboxSetupError {}

/// The settings files, in increasing precedence.
///
/// Same three the doctor reads, in the same order, so `/doctor`
/// cannot report a different `sandbox.enabled` than the one actually
/// in force — and the same three the harness reads to find out whether a
/// disabled sandbox plugin contradicts the settings, which is why the list
/// itself lives in `rebon-config` rather than here.
pub use rebon_config::settings_files;

/// Read and merge the `sandbox` block from the settings files.
///
/// Later files win **whole-block**, not key-by-key. A project that
/// declares a sandbox block is describing the sandbox it needs, and
/// merging a user-level `allowWrite` into it would widen that project's
/// write scope with a path the project never approved.
pub fn load_settings(
    config_dir: &Path,
    cwd: &Path,
) -> Result<(SandboxSettings, Vec<String>), SandboxSetupError> {
    let mut settings = SandboxSettings::default();
    let mut notes = Vec::new();

    for (label, path) in settings_files(config_dir, cwd) {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                notes.push(format!(
                    "sandbox: could not read {label} settings {}: {err}",
                    path.display()
                ));
                continue;
            }
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|err| SandboxSetupError::BadSettings {
                path: path.display().to_string(),
                detail: format!("could not be parsed: {err}"),
            })?;
        if value.get("sandbox").is_none() {
            continue;
        }
        settings = SandboxSettings::from_settings_value(&value).map_err(|err| {
            SandboxSetupError::BadSettings {
                path: path.display().to_string(),
                detail: err.to_string(),
            }
        })?;
    }

    Ok((settings, notes))
}

/// Build the session's sandbox from settings on disk.
pub fn setup(config_dir: &Path, cwd: &Path) -> Result<SessionSandbox, SandboxSetupError> {
    let (settings, notes) = load_settings(config_dir, cwd)?;
    Ok(build(settings, cwd, notes))
}

/// Build from already-parsed settings.
///
/// Split from [`setup`] so the whole decision tree is testable
/// without writing settings files, and so the desktop app — which
/// gets its settings from somewhere else — can reuse it.
pub fn build(settings: SandboxSettings, cwd: &Path, notes: Vec<String>) -> SessionSandbox {
    build_with(settings, cwd, notes, SessionOptions::default())
}

/// What a caller can supply beyond the settings themselves.
#[derive(Default)]
pub struct SessionOptions {
    /// Anything that must outlive the build and die with the session. The
    /// loopback proxy goes here: it is started *before* the runtime, because
    /// its ports have to be in the session config the runtime compiles, and
    /// it must stop when the session stops — a listener outliving its session
    /// would keep answering with a policy nobody is enforcing any more.
    pub resources: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// Where macOS seatbelt denials go. Defaults to a `tracing` warning,
    /// which is already the difference between an auditable refusal and one
    /// that is discarded — see [`crate::runtime::macos_monitor`].
    pub violation_sink: Option<Arc<dyn crate::runtime::macos_monitor::ViolationSink>>,
}

/// [`build`], with resources and a violation sink.
pub fn build_with(
    settings: SandboxSettings,
    cwd: &Path,
    mut notes: Vec<String>,
    options: SessionOptions,
) -> SessionSandbox {
    let SessionOptions {
        resources,
        violation_sink,
    } = options;
    let platform = current_platform();
    if sandbox_win_override_ignored(settings.mode) {
        notes.push(
            "sandbox: REBON_SANDBOX_WIN_PATH is ignored in strict mode — the helper is taken from \
             its installed location. Set `\"mode\": \"relaxed\"` to point at a local build."
                .to_string(),
        );
    }
    let sandbox_win = probe_sandbox_win(settings.mode);
    let sandbox_win_path = if sandbox_win.binary_present {
        find_sandbox_win(settings.mode)
    } else {
        None
    };
    let support = probe_support(
        platform,
        settings.enabled,
        &PathLookup,
        sandbox_win,
        sandbox_win_path,
    );

    if !settings.enabled {
        return SessionSandbox {
            enabled: false,
            platform,
            override_mode: override_mode(&settings),
            excluded_commands: settings.excluded_commands,
            runtime: None,
            support,
            notes,
        };
    }

    if !has_backend(platform) {
        // Not an error: the user asked for a sandbox on a platform
        // that has none, and refusing to start would leave them
        // unable to use Rebon at all. The policy stays inactive, so
        // nothing claims the commands are confined.
        notes.push(format!(
            "sandbox: enabled in settings but this platform ({}) has no sandbox backend; \
             commands will run unconfined",
            platform.as_wire()
        ));
        return SessionSandbox {
            enabled: false,
            platform,
            override_mode: override_mode(&settings),
            excluded_commands: settings.excluded_commands,
            runtime: None,
            support,
            notes,
        };
    }

    let mut session = settings.session.clone();
    // The project directory is writable by default. Without it the
    // very first command in a sandboxed session fails to write a file
    // in the directory the user is working in, which reads as the
    // sandbox being broken rather than configured.
    if session.filesystem.allow_write.is_empty() {
        session.filesystem.allow_write.push(cwd.to_path_buf());
    }

    let probe: Arc<dyn crate::runtime::ConfinedProbe> = match platform {
        SandboxPlatform::Windows => Arc::new(PlatformProbe::windows(
            support.sandbox_win.is_ready(),
            support.sandbox_win.remediation().join(" "),
        )),
        _ => Arc::new(PlatformProbe::linux(support.bwrap_path.clone())),
    };

    // One tag, used twice: it is embedded in the seatbelt profile so every
    // denial message ends with it, and it is the predicate the monitor
    // selects on. Generated here rather than inside the runtime because the
    // monitor has to be started with the same value.
    let log_tag = session_log_tag();

    // RFC §5.3. Only macOS produces these; `Monitor::start` is inert
    // elsewhere, so the call is unconditional and the platform check lives
    // in one place.
    //
    // Failing to start is a note, not a refusal: losing the explanation for
    // a denial is bad, and refusing to open a session over it would be
    // worse.
    let monitor = match crate::runtime::macos_monitor::Monitor::start(
        &log_tag,
        violation_sink.unwrap_or_else(default_violation_sink),
    ) {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            notes.push(format!(
                "sandbox: could not watch for seatbelt denials ({error}); a command refused by \
                 the sandbox will fail without saying which rule refused it"
            ));
            None
        }
    };

    let runtime = Arc::new(SandboxRuntime::new(SandboxRuntimeInit {
        platform,
        session,
        mode: settings.mode,
        log_tag,
        debug_session: is_debug_session(),
        support: support.clone(),
        probe,
        // Both ride along so both die with the session.
        session_resources: Some(Arc::new((resources, monitor))),
    }));

    SessionSandbox {
        enabled: true,
        platform,
        override_mode: override_mode(&settings),
        excluded_commands: settings.excluded_commands,
        runtime: Some(runtime),
        support,
        notes,
    }
}

/// `allowUnsandboxedCommands` in settings, as the override mode the
/// tools read.
///
/// The two spell the same idea in opposite directions — `Open` is the
/// permissive one and corresponds to `allowUnsandboxedCommands: true`
/// — so the mapping lives in one place rather than being re-derived
/// at each use.
fn override_mode(settings: &SandboxSettings) -> OverrideMode {
    if settings.allow_unsandboxed_commands {
        OverrideMode::Open
    } else {
        OverrideMode::Closed
    }
}

/// Where seatbelt denials go when the caller did not say.
///
/// A `tracing` warning rather than a silent drop. The command that was
/// refused reports whatever its own tooling said — usually a bare
/// "permission denied", often nothing — and the fact that the sandbox
/// refused it exists only here.
fn default_violation_sink() -> Arc<dyn crate::runtime::macos_monitor::ViolationSink> {
    Arc::new(|violation: crate::runtime::macos::violations::Violation| {
        tracing::warn!(
            operation = %violation.operation,
            "sandbox denied an operation: {}",
            violation.raw
        );
    })
}

/// A per-process tag for correlating macOS sandbox denials.
///
/// Built from the process id and the start time rather than a random
/// number so it needs no RNG dependency and is still distinct between
/// two Rebon processes on one machine, which is all the log-stream
/// predicate requires.
fn session_log_tag() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    crate::runtime::macos::session_log_tag(&std::process::id().to_string(), nanos)
}

/// Whether the agent is running under a debugger.
///
/// A debugger needs the ptrace the sandbox drops, so strict mode's
/// confinement probe is skipped. Gated on an explicit environment
/// variable rather than sniffing for a debugger: the exemption
/// weakens a security check, and it should take a deliberate act to
/// turn on.
fn is_debug_session() -> bool {
    std::env::var("REBON_SANDBOX_DEBUG")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Which helper binary `REBON_SANDBOX_WIN_PATH` is allowed to select.
///
/// **The variable is a strict-mode bypass, which is why strict mode drops it**
/// — RFC §11.3, §13 item 2. It grants no privilege: `exec` never elevates, so
/// pointing it elsewhere cannot make a command more powerful than the user
/// already is. What it decides is *which binary is the thing claiming to
/// confine your commands*. Point it at a stub that only calls
/// `CreateProcess`, and every command runs with the caller's full rights
/// while Rebon reports a confined session — which is precisely the state
/// strict mode's confinement probe exists to refuse. An environment variable
/// is also the easiest thing in the world for something else on the machine
/// to have set.
///
/// In relaxed mode it stays honoured, because that is the mode whose whole
/// meaning is "confinement may not happen and I accept that", and pointing at
/// a local build is why a developer sets it in the first place.
///
/// Pure so the rule is testable on any platform without touching the
/// process environment, which two tests running at once would race on.
pub fn resolve_sandbox_win_override(mode: SandboxMode, configured: Option<&str>) -> Option<String> {
    let configured = configured.filter(|value| !value.is_empty())?;
    match mode {
        SandboxMode::Strict => None,
        SandboxMode::Relaxed => Some(configured.to_string()),
    }
}

/// Whether an override was set and is being disregarded.
///
/// Derived from [`resolve_sandbox_win_override`] rather than re-deriving the
/// condition, so the note in [`build`] cannot say "ignored" about a path that
/// was in fact used.
pub fn is_sandbox_win_override_ignored(mode: SandboxMode, configured: Option<&str>) -> bool {
    configured.is_some_and(|value| !value.is_empty())
        && resolve_sandbox_win_override(mode, configured).is_none()
}

fn configured_sandbox_win_override() -> Option<String> {
    std::env::var("REBON_SANDBOX_WIN_PATH").ok()
}

fn sandbox_win_override_ignored(mode: SandboxMode) -> bool {
    is_sandbox_win_override_ignored(mode, configured_sandbox_win_override().as_deref())
}

/// Probe this machine exactly the way [`build`] does — RFC §10.
///
/// Exists so `/doctor` and `/sandbox` can report the *same* verdict the
/// executor will act on. They used to re-derive it: their own `PATH` walk for
/// `rg`/`bwrap`/`socat`, an empty warnings list, and the UI's
/// `SandboxPlatform::is_supported()` predicate — which excludes Windows, so
/// the Windows helper's remediation was computed, stored, and displayed
/// nowhere.
///
/// Reading the live session's report instead would be the other way to fix
/// it, but it needs the report plumbed through several layers to reach a
/// command handler, and a doctor is supposed to answer "what does this
/// machine have **now**". Sharing the function rather than the value gives
/// that, and still makes it impossible for the two to disagree about the
/// logic.
///
/// Deliberately *not* [`setup`]: that builds a session, which starts a proxy
/// and a log monitor. A diagnostic must not do either.
pub fn probe_machine(mode: SandboxMode, sandbox_enabled: bool) -> SupportReport {
    let sandbox_win = probe_sandbox_win(mode);
    let sandbox_win_path = if sandbox_win.binary_present {
        find_sandbox_win(mode)
    } else {
        None
    };
    probe_support(
        current_platform(),
        sandbox_enabled,
        &PathLookup,
        sandbox_win,
        sandbox_win_path,
    )
}

#[cfg(not(windows))]
fn probe_sandbox_win(_mode: SandboxMode) -> SandboxWinStatus {
    SandboxWinStatus::default()
}

#[cfg(not(windows))]
fn find_sandbox_win(_mode: SandboxMode) -> Option<PathBuf> {
    None
}

/// Ask the Windows helper what state it is in.
///
/// Only the binary's presence is checked here. The other answers — contract
/// version, account provisioned, credentials stored, WFP filters installed —
/// live inside the helper's own `status` output, and guessing at them from
/// this side would produce remediation advice for problems the user may not
/// have.
#[cfg(windows)]
fn probe_sandbox_win(mode: SandboxMode) -> SandboxWinStatus {
    let Some(path) = find_sandbox_win(mode) else {
        return SandboxWinStatus::default();
    };
    let mut status = SandboxWinStatus {
        binary_present: true,
        ..Default::default()
    };
    let Ok(output) = std::process::Command::new(&path)
        .arg("status")
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return status;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    status.version = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("version=")
            .and_then(|value| value.trim().parse().ok())
    });
    status.user_provisioned = text.contains("user=ok");
    status.credentials_present = text.contains("credentials=ok");
    status.wfp_installed = text.contains("wfp=ok");
    status
}

#[cfg(windows)]
fn find_sandbox_win(mode: SandboxMode) -> Option<PathBuf> {
    let explicit = resolve_sandbox_win_override(mode, configured_sandbox_win_override().as_deref());
    let executable = std::env::current_exe().ok();
    crate::runtime::windows::sandbox_win_candidates(explicit.as_deref(), executable.as_deref())
        .into_iter()
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: serde_json::Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        (temp, config, cwd)
    }

    #[test]
    fn no_settings_files_means_the_sandbox_is_off() {
        let (_temp, config, cwd) = dirs();

        let setup = setup(&config, &cwd).unwrap();

        assert!(!setup.enabled);
        assert!(!setup.enabled);
        assert!(setup.runtime.is_none());
        assert!(setup.notes.is_empty());
    }

    #[test]
    fn a_settings_file_without_a_sandbox_block_leaves_it_off() {
        let (_temp, config, cwd) = dirs();
        write(&config.join("settings.json"), json!({"theme": "dark"}));

        assert!(!setup(&config, &cwd).unwrap().enabled);
    }

    #[test]
    fn a_malformed_settings_file_stops_the_session() {
        let (_temp, config, cwd) = dirs();
        std::fs::write(config.join("settings.json"), b"{ not json").unwrap();

        let error = setup(&config, &cwd).unwrap_err();

        assert!(matches!(error, SandboxSetupError::BadSettings { .. }));
        assert!(error.to_string().contains("could not be parsed"));
    }

    #[test]
    fn a_wrong_typed_sandbox_key_stops_the_session() {
        let (_temp, config, cwd) = dirs();
        write(
            &config.join("settings.json"),
            json!({"sandbox": {"enabled": "yes"}}),
        );

        let error = setup(&config, &cwd).unwrap_err();

        assert!(error.to_string().contains("sandbox.enabled"));
    }

    #[test]
    fn the_project_block_replaces_the_user_block_rather_than_merging() {
        let (_temp, config, cwd) = dirs();
        write(
            &config.join("settings.json"),
            json!({"sandbox": {"enabled": true, "filesystem": {"allowWrite": ["/user-root"]}}}),
        );
        write(
            &cwd.join(".rebon").join("settings.json"),
            json!({"sandbox": {"enabled": true, "filesystem": {"allowWrite": ["/project-root"]}}}),
        );

        let (settings, _) = load_settings(&config, &cwd).unwrap();

        assert_eq!(
            settings.session.filesystem.allow_write,
            vec![PathBuf::from("/project-root")],
            "a user-level write root must not leak into a project that declared its own"
        );
    }

    #[test]
    fn local_settings_win_over_project_settings() {
        let (_temp, config, cwd) = dirs();
        write(
            &cwd.join(".rebon").join("settings.json"),
            json!({"sandbox": {"enabled": true}}),
        );
        write(
            &cwd.join(".rebon").join("settings.local.json"),
            json!({"sandbox": {"enabled": false}}),
        );

        let (settings, _) = load_settings(&config, &cwd).unwrap();

        assert!(!settings.enabled);
    }

    #[test]
    fn an_enabled_sandbox_gets_a_runtime_where_a_backend_exists() {
        let (_temp, _config, cwd) = dirs();
        let settings = SandboxSettings {
            enabled: true,
            ..Default::default()
        };

        let setup = build(settings, &cwd, Vec::new());

        if has_backend(current_platform()) {
            assert!(setup.enabled);
            assert!(setup.runtime.is_some());
            assert!(setup.enabled);
        } else {
            assert!(!setup.enabled);
            assert!(!setup.notes.is_empty());
        }
    }

    #[test]
    fn the_project_directory_is_writable_by_default() {
        let (_temp, _config, cwd) = dirs();
        if !has_backend(current_platform()) {
            return;
        }
        let settings = SandboxSettings {
            enabled: true,
            ..Default::default()
        };

        let setup = build(settings, &cwd, Vec::new());
        let runtime = setup.runtime.unwrap();

        assert_eq!(
            runtime.session_config().filesystem.allow_write,
            vec![cwd.clone()]
        );
    }

    #[test]
    fn a_configured_write_root_is_not_widened_with_the_project_directory() {
        let (_temp, _config, cwd) = dirs();
        if !has_backend(current_platform()) {
            return;
        }
        let mut settings = SandboxSettings {
            enabled: true,
            ..Default::default()
        };
        settings.session.filesystem.allow_write = vec![PathBuf::from("/only-this")];

        let setup = build(settings, &cwd, Vec::new());
        let runtime = setup.runtime.unwrap();

        assert_eq!(
            runtime.session_config().filesystem.allow_write,
            vec![PathBuf::from("/only-this")],
            "a project that named its roots must not silently get another one"
        );
    }

    #[test]
    fn allow_unsandboxed_commands_maps_to_the_open_override() {
        let permissive = SandboxSettings {
            allow_unsandboxed_commands: true,
            ..Default::default()
        };
        assert_eq!(override_mode(&permissive), OverrideMode::Open);

        assert_eq!(
            override_mode(&SandboxSettings::default()),
            OverrideMode::Closed,
            "the restrictive mode is the default"
        );
    }

    #[test]
    fn excluded_commands_reach_the_policy() {
        let (_temp, _config, cwd) = dirs();
        let settings = SandboxSettings {
            enabled: true,
            excluded_commands: vec!["git".into()],
            ..Default::default()
        };

        let setup = build(settings, &cwd, Vec::new());

        // The list is carried, not applied: matching a command
        // against it is the tool layer's job, since that is where a
        // command string exists.
        assert_eq!(setup.excluded_commands, vec!["git"]);
    }

    #[test]
    fn a_disabled_sandbox_still_reports_its_excluded_commands() {
        let (_temp, _config, cwd) = dirs();
        let settings = SandboxSettings {
            enabled: false,
            excluded_commands: vec!["npm".into()],
            ..Default::default()
        };

        let setup = build(settings, &cwd, Vec::new());

        assert_eq!(setup.excluded_commands, vec!["npm"]);
        assert!(!setup.enabled);
    }

    #[test]
    fn a_disabled_sandbox_probes_no_dependencies() {
        let (_temp, _config, cwd) = dirs();

        let setup = build(SandboxSettings::default(), &cwd, Vec::new());

        assert!(setup.support.errors.is_empty());
        assert!(setup.support.bwrap_path.is_none());
    }

    #[test]
    fn the_settings_file_list_matches_the_doctor_order() {
        let (_temp, config, cwd) = dirs();
        let files = settings_files(&config, &cwd);

        assert_eq!(files.len(), 3);
        assert_eq!(files[0].0, "user");
        assert_eq!(files[1].0, "project");
        assert_eq!(files[2].0, "local");
        assert_eq!(files[1].1, cwd.join(".rebon").join("settings.json"));
    }

    #[test]
    fn strict_mode_ignores_the_helper_path_override() {
        // RFC §11.3 / §13 item 2. The variable does not grant a privilege —
        // `exec` never elevates — it decides *which binary claims to be
        // confining the command*. A stub that only calls `CreateProcess`
        // would run everything unconfined while Rebon reports success, which
        // is the exact state strict mode exists to refuse.
        assert_eq!(
            resolve_sandbox_win_override(SandboxMode::Strict, Some(r"D:\evil\sandbox-win.exe")),
            None
        );
    }

    #[test]
    fn relaxed_mode_still_honours_the_helper_path_override() {
        // Relaxed mode's whole meaning is that confinement may not happen,
        // and pointing at a local build is why a developer sets this.
        assert_eq!(
            resolve_sandbox_win_override(SandboxMode::Relaxed, Some(r"D:\build\sandbox-win.exe")),
            Some(r"D:\build\sandbox-win.exe".to_string())
        );
    }

    #[test]
    fn an_empty_override_is_the_same_as_none_in_both_modes() {
        for mode in [SandboxMode::Strict, SandboxMode::Relaxed] {
            assert_eq!(resolve_sandbox_win_override(mode, Some("")), None);
            assert_eq!(resolve_sandbox_win_override(mode, None), None);
            assert!(!is_sandbox_win_override_ignored(mode, Some("")));
            assert!(!is_sandbox_win_override_ignored(mode, None));
        }
    }

    #[test]
    fn only_a_dropped_override_is_reported_as_ignored() {
        assert!(is_sandbox_win_override_ignored(
            SandboxMode::Strict,
            Some(r"D:\build\sandbox-win.exe")
        ));
        assert!(!is_sandbox_win_override_ignored(
            SandboxMode::Relaxed,
            Some(r"D:\build\sandbox-win.exe")
        ));
    }

    #[test]
    fn an_unreadable_settings_file_is_a_note_not_a_refusal() {
        let (_temp, config, cwd) = dirs();
        // A directory where a file is expected: readable path, but
        // `read` fails with something other than NotFound.
        std::fs::create_dir_all(config.join("settings.json")).unwrap();

        let setup = setup(&config, &cwd).unwrap();

        assert!(!setup.notes.is_empty());
        assert!(!setup.enabled);
    }
}
