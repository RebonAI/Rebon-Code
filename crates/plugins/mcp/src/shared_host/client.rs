//! A session's side: finding or starting the host, attaching to a server
//! through it, and the blocking controls the desktop app calls.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use super::protocol::{
    host_dir, load_host_info, Hello, Reply, Request, ServerSpec, SharedServerInfo, StopReason,
    HOST_SCHEMA, LOG_FILE,
};
use crate::mcp_stdio::StdioServerConfig;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const HOST_START_TIMEOUT: Duration = Duration::from_secs(10);
const HOST_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Variables that say which session this is. A shared server serves more
/// than one, so it is started without them.
const SESSION_ENV: &[&str] = &["REBON_SESSION_ID"];

/// Variables that decide which program a command is and what it reads; two
/// sessions that differ in one of these get separate servers.
const KEY_ENV: &[&str] = &["PATH", "PATHEXT", "HOME", "USERPROFILE", "REBON_CONFIG_DIR"];

/// How to start the host: a program that runs [`super::run_host`].
#[derive(Debug, Clone)]
pub struct HostLauncher {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

static LAUNCHER: OnceLock<HostLauncher> = OnceLock::new();

/// Turn sharing on for this process. Until a launcher is installed every
/// stdio server is started by the session itself, as before.
pub fn install_launcher(launcher: HostLauncher) {
    let _ = LAUNCHER.set(launcher);
}

fn sharing_switched_off() -> bool {
    std::env::var("REBON_MCP_NO_SHARE").is_ok_and(|value| {
        let value = value.trim();
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

/// Rebon's own servers know which session started them, from the
/// environment; one of those serving several sessions would answer for the
/// wrong one.
fn is_rebon_server(command: &str) -> bool {
    Path::new(command)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.to_ascii_lowercase().starts_with("rebon"))
}

/// Whether this server goes through the host in this process.
pub(crate) fn should_share(config: &StdioServerConfig) -> bool {
    config.shared
        && LAUNCHER.get().is_some()
        && !sharing_switched_off()
        && !is_rebon_server(&config.command)
}

fn env_name_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

pub(crate) fn spec_for(config: &StdioServerConfig) -> std::io::Result<ServerSpec> {
    let here = std::env::current_dir()?;
    let cwd = match &config.cwd {
        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
        Some(cwd) => here.join(cwd),
        None => here,
    };
    let mut env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| !SESSION_ENV.iter().any(|bound| env_name_eq(name, bound)))
        .collect();
    let mut key_env: Vec<(String, String)> = env
        .iter()
        .filter(|(name, _)| KEY_ENV.iter().any(|wanted| env_name_eq(name, wanted)))
        .map(|(name, value)| (name.to_ascii_uppercase(), value.clone()))
        .collect();
    for (name, value) in &config.env {
        env.insert(name.clone(), value.clone());
        key_env.push((format!("config:{name}"), value.clone()));
    }
    Ok(ServerSpec {
        name: config.name.clone(),
        command: config.command.clone(),
        args: config.args.clone(),
        env: env.into_iter().collect(),
        cwd: cwd.to_string_lossy().into_owned(),
        key_env,
        init_timeout_ms: config.effective_timeout().as_millis() as u64,
    })
}

#[derive(Debug)]
pub(crate) enum AttachError {
    /// No host to be had: the caller starts the server itself.
    Unavailable(String),
    /// The host is there and the server would not start. Starting it
    /// directly would fail the same way, so this is the session's answer.
    Server(String),
}

pub(crate) struct Attached {
    pub reader: BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
    pub pid: Option<u32>,
}

/// Attach to `config` through the host, starting the host when there is none.
pub(crate) async fn attach(config: &StdioServerConfig) -> Result<Attached, AttachError> {
    let spec = spec_for(config).map_err(|error| AttachError::Unavailable(error.to_string()))?;
    let dir = host_dir().map_err(|error| AttachError::Unavailable(error.to_string()))?;
    attach_in(&dir, &spec, LAUNCHER.get()).await
}

pub(crate) async fn attach_in(
    dir: &Path,
    spec: &ServerSpec,
    launcher: Option<&HostLauncher>,
) -> Result<Attached, AttachError> {
    match try_attach(dir, spec).await {
        Err(AttachError::Unavailable(reason)) => {
            let Some(launcher) = launcher else {
                return Err(AttachError::Unavailable(reason));
            };
            launch_host(dir, launcher).map_err(AttachError::Unavailable)?;
        }
        done => return done,
    }
    let deadline = tokio::time::Instant::now() + HOST_START_TIMEOUT;
    loop {
        match try_attach(dir, spec).await {
            Err(AttachError::Unavailable(reason)) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(AttachError::Unavailable(format!(
                        "the shared MCP host did not come up: {reason}"
                    )));
                }
                tokio::time::sleep(HOST_POLL_INTERVAL).await;
            }
            done => return done,
        }
    }
}

async fn try_attach(dir: &Path, spec: &ServerSpec) -> Result<Attached, AttachError> {
    let info = load_host_info(dir)
        .ok_or_else(|| AttachError::Unavailable("no shared MCP host is running".into()))?;
    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect(("127.0.0.1", info.port)),
    )
    .await
    .map_err(|_| AttachError::Unavailable("connecting to the shared MCP host timed out".into()))?
    .map_err(|error| AttachError::Unavailable(format!("shared MCP host: {error}")))?;
    let (read, mut writer) = stream.into_split();
    let hello = Hello {
        schema: HOST_SCHEMA,
        token: info.token,
        request: Request::Attach { spec: spec.clone() },
    };
    let mut line =
        serde_json::to_vec(&hello).map_err(|error| AttachError::Unavailable(error.to_string()))?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .map_err(|error| AttachError::Unavailable(format!("shared MCP host: {error}")))?;
    let mut reader = BufReader::new(read);
    let mut answer = String::new();
    // The host starts the server before it answers, so this waits as long as
    // a direct start would, and a little more.
    let wait = Duration::from_millis(spec.init_timeout_ms) + CONNECT_TIMEOUT;
    let read = tokio::time::timeout(wait, reader.read_line(&mut answer))
        .await
        .map_err(|_| AttachError::Server("request `initialize` timed out".into()))?
        .map_err(|error| AttachError::Unavailable(format!("shared MCP host: {error}")))?;
    if read == 0 {
        return Err(AttachError::Unavailable(
            "the shared MCP host closed the connection".into(),
        ));
    }
    let answer: Reply = serde_json::from_str(answer.trim())
        .map_err(|error| AttachError::Unavailable(format!("shared MCP host: {error}")))?;
    if let Some(error) = answer.error {
        return Err(AttachError::Server(error));
    }
    Ok(Attached {
        reader,
        writer,
        pid: answer.pid,
    })
}

/// Start a host candidate. Losing candidates leave on their own, so a race
/// between two sessions costs one short-lived process.
fn launch_host(dir: &Path, launcher: &HostLauncher) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
        .map_err(|error| error.to_string())?;
    let build = |breakaway: bool| -> Result<std::process::Command, String> {
        let stderr = log.try_clone().map_err(|error| error.to_string())?;
        let stdout = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = std::process::Command::new(&launcher.program);
        command
            .args(&launcher.args)
            .env("REBON_MCP_HOST_DIR", dir)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        detach(&mut command, breakaway);
        Ok(command)
    };
    // Out of whatever job the session runs in, so the host outlives it; a job
    // that forbids that still gets a host, one that lasts as long as it does.
    let child = match build(true)?.spawn() {
        Ok(child) => child,
        Err(_) => build(false)?
            .spawn()
            .map_err(|error| format!("failed to start the shared MCP host: {error}"))?,
    };
    // Waited on so it never lingers as a zombie of this session.
    std::thread::Builder::new()
        .name("mcp-host-reaper".into())
        .spawn(move || {
            let mut child = child;
            let _ = child.wait();
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command, breakaway: bool) {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let mut flags = crate::mcp_stdio::CREATE_NO_WINDOW;
    if breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    command.creation_flags(flags);
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command, _breakaway: bool) {
    use std::os::unix::process::CommandExt;
    // Out of the session's process group, so stopping the session does not
    // stop every other session's servers.
    command.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn detach(_command: &mut std::process::Command, _breakaway: bool) {}

// --------------------------------------------------------------------------
// Controls
// --------------------------------------------------------------------------

/// The servers the host is running. Empty when no host is.
pub fn list_shared_servers() -> anyhow::Result<Vec<SharedServerInfo>> {
    list_in(&host_dir()?)
}

/// Stop the processes the host runs for the server called `name`: the one
/// running in `project` when given, else every one. Returns how many it
/// stopped; zero when no host is running.
///
/// A server with a `cwd` of its own runs there whichever project started
/// it, so naming a project never stops it.
pub fn stop_shared_server(
    name: &str,
    reason: StopReason,
    project: Option<&Path>,
) -> anyhow::Result<usize> {
    stop_in(&host_dir()?, name, reason, project)
}

/// Ask the host to stop all its servers and leave. `false` when none ran.
pub fn shut_down_shared_host() -> anyhow::Result<bool> {
    Ok(control(&host_dir()?, Request::Shutdown)?.is_some())
}

pub(crate) fn list_in(dir: &Path) -> anyhow::Result<Vec<SharedServerInfo>> {
    Ok(control(dir, Request::List)?
        .and_then(|reply| reply.servers)
        .unwrap_or_default())
}

pub(crate) fn stop_in(
    dir: &Path,
    name: &str,
    reason: StopReason,
    project: Option<&Path>,
) -> anyhow::Result<usize> {
    Ok(control(
        dir,
        Request::Stop {
            name: name.to_string(),
            reason,
            cwd: project.map(|path| path.to_string_lossy().into_owned()),
        },
    )?
    .and_then(|reply| reply.stopped)
    .unwrap_or(0))
}

/// One request to the host. `None` when there is no host to ask.
fn control(dir: &Path, request: Request) -> anyhow::Result<Option<Reply>> {
    let Some(info) = load_host_info(dir) else {
        return Ok(None);
    };
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], info.port));
    let Ok(stream) = std::net::TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) else {
        // An address left behind by a host that is gone.
        return Ok(None);
    };
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut line = serde_json::to_vec(&Hello {
        schema: HOST_SCHEMA,
        token: info.token,
        request,
    })?;
    line.push(b'\n');
    writer.write_all(&line)?;
    let mut answer = String::new();
    std::io::BufReader::new(stream).read_line(&mut answer)?;
    let reply: Reply = serde_json::from_str(answer.trim())?;
    if let Some(error) = reply.error {
        anyhow::bail!("shared MCP host: {error}");
    }
    Ok(Some(reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebon_servers_are_never_shared() {
        assert!(is_rebon_server("rebon"));
        assert!(is_rebon_server("C:/Program Files/Rebon/rebon-cli.exe"));
        assert!(is_rebon_server("/usr/local/bin/rebon-browser-mcp"));
        assert!(!is_rebon_server("npx"));
        assert!(!is_rebon_server("/opt/reborn/server"));
    }

    #[test]
    fn a_spec_has_the_session_environment_without_who_the_session_is() {
        let mut config = StdioServerConfig::new("fs", "npx", vec!["server".into()]);
        config.env = vec![("TOKEN".into(), "secret".into())];
        config.cwd = Some("sub".into());
        // Read, not set: the test binary's environment is shared.
        let spec = spec_for(&config).unwrap();
        assert!(spec.env.iter().any(|(k, v)| k == "TOKEN" && v == "secret"));
        assert!(!spec.env.iter().any(|(k, _)| k == "REBON_SESSION_ID"));
        assert!(spec
            .key_env
            .iter()
            .any(|(k, v)| k == "config:TOKEN" && v == "secret"));
        let here = std::env::current_dir().unwrap();
        assert_eq!(PathBuf::from(&spec.cwd), here.join("sub"));
        assert!(spec
            .key_env
            .iter()
            .all(|(k, _)| k.starts_with("config:") || KEY_ENV.contains(&k.as_str())));
    }

    #[test]
    fn no_host_means_nothing_to_list_or_stop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_in(dir.path()).unwrap().is_empty());
        assert_eq!(
            stop_in(dir.path(), "fs", StopReason::Disabled, None).unwrap(),
            0
        );
    }
}
