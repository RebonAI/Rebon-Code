//! The `container-sandbox` seat: a proxy for each plugin container, and the
//! OS sandbox around its host where this machine has one.
//!
//! A container's host already runs under Node's permission model (its own
//! files only, no processes). What that model does not touch is the network,
//! so every container gets a proxy of its own admitting exactly the hosts the
//! person granted it — and none, when it was granted none: a closed
//! allowlist naming a host that cannot exist refuses everything. The host is
//! pointed at the proxy through the standard variables, which Node's `fetch`
//! honours (`NODE_USE_ENV_PROXY`, set by the container itself).
//!
//! Where this machine's OS sandbox is usable the host runs under it too —
//! bubblewrap, seatbelt or `sandbox-win.exe` — writing only its data
//! directory and reaching nothing but its proxy. Without the OS layer the
//! proxy is advisory: a plugin opening a raw socket bypasses it, and the
//! confinement says so in its notes rather than claiming a restriction that
//! is not in force.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rebon_tool::{
    BinShell, ConfineRequest, ConfinedLauncher, ContainerConfinement, ContainerSandboxSource,
};

use crate::proxy::{self, DomainPolicy};
use crate::runtime::config::RuntimePaths;
use crate::runtime::env::proxy_env;
use crate::runtime::support::{current_platform, has_backend};
use crate::runtime::windows::{parse_status_features, SandboxWinExecOptions};
use crate::runtime::{
    CommandRequest, ConfinedProbe, PlatformProbe, SandboxMode, SandboxRuntime, SandboxRuntimeInit,
    SessionSandboxConfig,
};
use crate::view::platform::SandboxPlatform;

/// The one name a closed allowlist holds when nothing was granted. `.invalid`
/// is reserved never to resolve, so admitting it admits nothing.
const NOTHING_GRANTED: &str = "nothing.invalid";

/// The seat's provider.
pub struct ContainerSandbox;

impl ContainerSandbox {
    /// The domain policy one container's proxy enforces.
    pub fn policy(network: &[String]) -> DomainPolicy {
        if network.iter().any(|host| host == "*") {
            // Any host: an open policy, still through the proxy.
            return DomainPolicy::new(Vec::<String>::new(), Vec::<String>::new());
        }
        if network.is_empty() {
            DomainPolicy::new([NOTHING_GRANTED], Vec::<String>::new())
        } else {
            DomainPolicy::new(network, Vec::<String>::new())
        }
    }
}

impl ContainerSandboxSource for ContainerSandbox {
    fn confine(&self, request: &ConfineRequest) -> Result<ContainerConfinement, String> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "no async runtime to run the container's network proxy on".to_owned())?;
        let platform = current_platform();
        // bubblewrap unshares the network, so on Linux the host reaches its
        // proxy through Unix sockets bridged into the sandbox.
        let socket_dir = (platform == SandboxPlatform::Linux).then(|| {
            std::env::temp_dir().join(format!(
                "rebon-container-{}-{}",
                std::process::id(),
                safe(&request.container)
            ))
        });
        if let Some(dir) = &socket_dir {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("creating {}: {error}", dir.display()))?;
        }
        let proxy = proxy::start_blocking(
            Self::policy(&request.network),
            socket_dir.as_deref(),
            &handle,
        )
        .map_err(|error| error.to_string())?;
        let endpoints = proxy.endpoints().clone();
        let paths = RuntimePaths {
            http_proxy_port: Some(endpoints.http_port),
            socks_proxy_port: Some(endpoints.socks_port),
            http_proxy_socket: endpoints.http_socket.clone(),
            socks_proxy_socket: endpoints.socks_socket.clone(),
            ..RuntimePaths::default()
        };
        let proxy_vars = proxy_env(&paths);
        let environment: Vec<(OsString, OsString)> = proxy_vars
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        let mut notes = Vec::new();
        let launcher = match os_launcher(request, platform, &paths, &proxy_vars) {
            Ok(launcher) => Some(launcher),
            Err(why) => {
                notes.push(format!(
                    "{}: no OS sandbox around the host ({why}); its network goes through a \
                     proxy admitting {}, which a plugin opening a raw socket bypasses",
                    request.container,
                    if request.network.is_empty() {
                        "no host".to_owned()
                    } else {
                        request.network.join(", ")
                    }
                ));
                None
            }
        };
        Ok(ContainerConfinement {
            launcher,
            environment,
            keep_alive: Some(Arc::new(proxy)),
            notes,
        })
    }
}

/// A container id as a directory name.
fn safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A path as the OS backends want it: absolute, no Windows verbatim prefix.
fn plain(path: &Path) -> PathBuf {
    let shown = path.to_string_lossy();
    match shown.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// What `sandbox-win.exe status` says, for the features it advertises.
fn helper_status(path: &Path) -> String {
    std::process::Command::new(path)
        .arg("status")
        .stdin(std::process::Stdio::null())
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

/// The OS sandbox around one container's host, or why there is none.
///
/// The rules are the container's: write its data directory, connect to
/// nothing but its proxy. Reads are Node's permission model's job — an OS
/// read jail tight enough to matter would have to know every library Node
/// loads — except on Windows, where the host runs as another account and
/// must be granted what it reads at all.
fn os_launcher(
    request: &ConfineRequest,
    platform: SandboxPlatform,
    paths: &RuntimePaths,
    proxy_vars: &[(String, String)],
) -> Result<ConfinedLauncher, String> {
    if !has_backend(platform) {
        return Err(format!("{} has no OS sandbox", platform.as_wire()));
    }
    let support = crate::runtime::session::probe_machine(SandboxMode::Strict, true);
    if !support.is_usable() {
        return Err(format!(
            "the OS sandbox is not usable here: {}",
            support.errors.join("; ")
        ));
    }
    let features = support
        .sandbox_win_path
        .as_deref()
        .filter(|_| platform == SandboxPlatform::Windows)
        .map(|path| parse_status_features(&helper_status(path)))
        .unwrap_or_default();
    wrap_host(request, platform, support, features, paths, proxy_vars)
}

/// The confinement half of [`os_launcher`], given what the probe found.
fn wrap_host(
    request: &ConfineRequest,
    platform: SandboxPlatform,
    support: crate::runtime::support::SupportReport,
    features: crate::runtime::windows::SandboxWinFeatures,
    paths: &RuntimePaths,
    proxy_vars: &[(String, String)],
) -> Result<ConfinedLauncher, String> {
    let windows = if platform == SandboxPlatform::Windows {
        if !(features.supports_pipe_stdin && features.supports_allow_read) {
            return Err(
                "sandbox-win.exe is too old to host a plugin (it needs pipe-stdin and allow-read)"
                    .to_owned(),
            );
        }
        let mut read: Vec<PathBuf> = request.node.parent().map(plain).into_iter().collect();
        for root in &request.read {
            let root = plain(root);
            if !read.contains(&root) {
                read.push(root);
            }
        }
        Some(SandboxWinExecOptions {
            pipe_stdin: true,
            allow_read: read,
            features,
        })
    } else {
        None
    };

    let mut session = SessionSandboxConfig::default();
    session.filesystem.allow_write = vec![plain(&request.write)];
    let any_host = request.network.iter().any(|host| host == "*");
    session.network.allowed_domains = if any_host {
        // Any host: the network stays open, and only files are confined.
        Vec::new()
    } else if request.network.is_empty() {
        vec![NOTHING_GRANTED.to_owned()]
    } else {
        request.network.clone()
    };
    session.runtime = paths.clone();
    let probe: Arc<dyn ConfinedProbe> = match platform {
        SandboxPlatform::Windows => Arc::new(PlatformProbe::windows(
            support.sandbox_win.is_ready(),
            support.sandbox_win.remediation().join(" "),
        )),
        _ => Arc::new(PlatformProbe::linux(support.bwrap_path.clone())),
    };
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let runtime = SandboxRuntime::new(SandboxRuntimeInit {
        platform,
        session,
        mode: SandboxMode::Strict,
        log_tag: crate::runtime::macos::session_log_tag(&request.container, started),
        debug_session: false,
        support,
        probe,
        session_resources: None,
    });
    runtime
        .assert_confined(true)
        .map_err(|error| error.to_string())?;

    let argv: Vec<String> = request
        .argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let Some((payload, head)) = argv.split_last() else {
        return Err("the host has no command line".to_owned());
    };
    let Some((program, args)) = head.split_first() else {
        return Err("the host's command line is one word".to_owned());
    };
    let mut command = CommandRequest::new(
        payload.clone(),
        BinShell::new(program.clone(), args.to_vec()),
    )
    .with_cwd(plain(&request.write))
    .with_network_restriction(!any_host);
    command.command_id = request.container.clone();
    // Windows runs the host as another account and hands it only what is
    // named; elsewhere the variables pass through anyway, and naming them
    // again changes nothing.
    command.set_env_vars = request
        .environment
        .iter()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .chain(proxy_vars.iter().cloned())
        .collect();
    if let Some(options) = windows {
        command = command.with_sandbox_win(options);
    }
    let wrapped = runtime.wrap(&command).map_err(|error| error.to_string())?;
    if !wrapped.backend.is_confined() {
        return Err("the OS sandbox would not confine the host".to_owned());
    }
    Ok(ConfinedLauncher {
        program: PathBuf::from(wrapped.program),
        args: wrapped.args.into_iter().map(OsString::from).collect(),
        env_set: wrapped
            .env_set
            .into_iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect(),
        env_unset: wrapped.env_unset.into_iter().map(OsString::from).collect(),
        backend: format!("{:?}", wrapped.backend),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_granted_nothing_reaches_nothing() {
        let policy = ContainerSandbox::policy(&[]);
        assert!(policy.is_allowlist());
        assert!(!policy.decide("api.exa.ai").is_allowed());
        assert!(!policy.decide("localhost").is_allowed());
    }

    #[test]
    fn a_container_granted_any_host_reaches_any_host() {
        let policy = ContainerSandbox::policy(&["*".to_owned()]);
        assert!(policy.decide("example.org").is_allowed());
        assert!(policy.decide("api.exa.ai").is_allowed());
    }

    #[test]
    fn a_container_reaches_exactly_what_it_was_granted() {
        let policy = ContainerSandbox::policy(&["api.exa.ai".to_owned()]);
        assert!(policy.decide("api.exa.ai").is_allowed());
        assert!(!policy.decide("evil.example").is_allowed());
        assert!(
            !policy.decide("exa.ai").is_allowed(),
            "the apex was not granted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn confining_starts_a_proxy_and_points_the_host_at_it() {
        let confinement = ContainerSandbox
            .confine(&ConfineRequest {
                container: "mod-x".into(),
                node: "/n/node".into(),
                argv: vec!["/n/node".into()],
                environment: Vec::new(),
                read: Vec::new(),
                write: "/d".into(),
                network: Vec::new(),
            })
            .expect("confines");
        let proxy = confinement
            .environment
            .iter()
            .find(|(key, _)| key == "HTTPS_PROXY")
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .expect("the host is pointed at a proxy");
        assert!(proxy.starts_with("http://127.0.0.1:"), "{proxy}");
        let port: u16 = proxy.rsplit(':').next().unwrap().parse().unwrap();
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "the proxy listens while the confinement is alive"
        );
        assert!(confinement.keep_alive.is_some());
        assert!(
            !confinement.notes.is_empty(),
            "the missing OS layer is said"
        );
    }

    async fn first_line(port: u16, host: &str) -> Option<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        socket
            .write_all(format!("CONNECT {host}:80 HTTP/1.1\r\nHost: {host}:80\r\n\r\n").as_bytes())
            .await
            .ok()?;
        let mut buffer = vec![0u8; 512];
        let read =
            tokio::time::timeout(std::time::Duration::from_secs(3), socket.read(&mut buffer))
                .await
                .ok()?
                .ok()?;
        String::from_utf8_lossy(&buffer[..read])
            .lines()
            .next()
            .map(str::to_owned)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_proxy_refuses_an_ungranted_host_at_once() {
        let confinement = ContainerSandbox
            .confine(&ConfineRequest {
                container: "pkg-x".into(),
                node: "/n/node".into(),
                argv: Vec::new(),
                environment: Vec::new(),
                read: Vec::new(),
                write: "/d".into(),
                network: vec!["granted.invalid".into()],
            })
            .unwrap();
        let url = confinement
            .environment
            .iter()
            .find(|(key, _)| key == "HTTP_PROXY")
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .unwrap();
        let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
        let refused = first_line(port, "other.invalid").await;
        assert!(
            refused.as_deref().is_some_and(|line| line.contains(" 403")),
            "{refused:?}"
        );
    }

    fn windows_ready() -> crate::runtime::support::SupportReport {
        crate::runtime::support::SupportReport {
            platform: SandboxPlatform::Windows,
            sandbox_win: crate::runtime::windows::SandboxWinStatus {
                binary_present: true,
                version: Some(crate::runtime::windows::SUPPORTED_STATUS_VERSION),
                user_provisioned: true,
                credentials_present: true,
                wfp_installed: true,
            },
            sandbox_win_path: Some(PathBuf::from(r"C:\Rebon\sandbox-win.exe")),
            ..Default::default()
        }
    }

    fn host_request() -> ConfineRequest {
        ConfineRequest {
            container: "pkg-exa".into(),
            node: PathBuf::from(r"C:\node\node.exe"),
            argv: vec![
                r"C:\node\node.exe".into(),
                "--permission".into(),
                r"C:\rt\plugin-host\src\cli.mjs".into(),
            ],
            environment: vec![("HOME".into(), r"C:\data\pkg-exa".into())],
            read: vec![PathBuf::from(r"C:\rt"), PathBuf::from(r"C:\pkgs\exa")],
            write: PathBuf::from(r"C:\data\pkg-exa"),
            network: vec!["api.exa.ai".into()],
        }
    }

    fn proxy_paths() -> RuntimePaths {
        RuntimePaths {
            http_proxy_port: Some(7001),
            socks_proxy_port: Some(7002),
            ..RuntimePaths::default()
        }
    }

    #[test]
    fn on_windows_the_host_runs_under_the_helper_with_a_pipe_its_reads_and_no_network() {
        let features = crate::runtime::windows::SandboxWinFeatures {
            supports_pipe_stdin: true,
            supports_allow_read: true,
        };
        let proxy = vec![("HTTPS_PROXY".to_owned(), "http://127.0.0.1:7001".to_owned())];
        let launcher = wrap_host(
            &host_request(),
            SandboxPlatform::Windows,
            windows_ready(),
            features,
            &proxy_paths(),
            &proxy,
        )
        .expect("a ready helper confines the host");
        let args: Vec<String> = launcher
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(launcher.program, PathBuf::from(r"C:\Rebon\sandbox-win.exe"));
        let has = |flag: &str, value: &str| {
            args.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value)
        };
        assert!(args.contains(&"--pipe-stdin".to_owned()), "{args:?}");
        assert!(args.contains(&"--block-network".to_owned()), "{args:?}");
        assert!(has("--allow-read", r"C:\node"), "Node itself: {args:?}");
        assert!(has("--allow-read", r"C:\pkgs\exa"), "the package: {args:?}");
        assert!(
            has("--allow-write", r"C:\data\pkg-exa"),
            "its data: {args:?}"
        );
        assert!(
            has("--env", r"HOME=C:\data\pkg-exa"),
            "its environment: {args:?}"
        );
        assert!(
            has("--env", "HTTPS_PROXY=http://127.0.0.1:7001"),
            "its proxy: {args:?}"
        );
        let tail: Vec<&str> = args
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            vec![
                r"C:\node\node.exe",
                "--permission",
                r"C:\rt\plugin-host\src\cli.mjs"
            ],
            "Node's own command line ends it"
        );
    }

    #[test]
    fn an_old_helper_leaves_the_host_to_nodes_permissions_and_says_why() {
        let error = wrap_host(
            &host_request(),
            SandboxPlatform::Windows,
            windows_ready(),
            Default::default(),
            &proxy_paths(),
            &[],
        )
        .unwrap_err();
        assert!(error.contains("too old"), "{error}");
    }

    #[test]
    fn confining_outside_a_runtime_is_refused_not_unrestricted() {
        let refused = ContainerSandbox.confine(&ConfineRequest {
            container: "mod-x".into(),
            node: "/n/node".into(),
            argv: Vec::new(),
            environment: Vec::new(),
            read: Vec::new(),
            write: "/d".into(),
            network: Vec::new(),
        });
        assert!(refused.is_err());
    }
}
