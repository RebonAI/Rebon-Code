//! `rebon rc …`: the clap half. The binary parses it and hands over what
//! only the binary knows — its config home, its store, its executable, its
//! runtime flags, its launch policy, and the readers of files it owns.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use rebon_bridge::http_client::{DeviceCredentials, HttpBridgeApiClient, HttpClientConfig};
use rebon_bridge::remote_permission::RemotePermissionPolicy;
use rebon_session_host::{BackgroundRuntimeFields, BackgroundStore, SessionHostClient};

use crate::core::uplink::PermissionProjection;
use crate::files::RcDir;
use crate::host::{HostTiming, LaunchGate, LocalSessionHost};
use crate::ledger::Ledger;
use crate::login::{self, LoginCredential};
use crate::projects::{resolve_projects, ProjectSource};
use crate::serve::{serve, ServeConfig, ServeTiming};
use crate::session::{WorkContext, WorkTiming};
use crate::transport::{HttpEnvironmentApi, WsConnector};

/// The environment variable a token can be passed in, instead of stdin.
/// Never a command-line argument: those end up in shell history and in
/// every process listing.
pub const TOKEN_ENV: &str = "REBON_RC_TOKEN";

#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum RcCommand {
    /// Bind this machine to a Remote Control server.
    ///
    /// Reads a token from REBON_RC_TOKEN or, when that is unset, from one
    /// line of stdin: the server's bootstrap token (first device of a new
    /// instance), another device's access token (add this machine to that
    /// account), or a refresh token minted for this machine. Stores only
    /// the device's refresh token, owner-only, under the config home.
    Login(LoginArgs),
    /// Register this machine and serve Remote Control sessions until
    /// interrupted.
    ///
    /// Sessions run in Rebon's background workers, one per session, in the
    /// projects this machine advertises (`rc.projects` in config.json, and
    /// `--project`; the current directory when neither is given).
    /// Everything a served session does is sent to the server in plain
    /// text.
    Serve(ServeArgs),
    /// Show what this machine is bound to, registered as, and serving.
    Status(StatusArgs),
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct LoginArgs {
    /// The RC server's API origin, e.g. https://rc.example.com.
    #[arg(long)]
    pub server: String,
    /// What the token is.
    #[arg(long, value_enum, default_value_t = TokenKind::Bootstrap)]
    pub token_kind: TokenKind,
    /// A name for this device on the server; the machine name by default.
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TokenKind {
    Bootstrap,
    Access,
    Refresh,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct ServeArgs {
    /// A project directory to serve, in addition to `rc.projects`.
    /// Repeatable.
    #[arg(long = "project", value_name = "DIR")]
    pub projects: Vec<PathBuf>,
    /// How many sessions run at once.
    #[arg(long, default_value_t = 8)]
    pub max_sessions: usize,
    /// The name the server shows for this machine.
    #[arg(long)]
    pub machine_name: Option<String>,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct StatusArgs {
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// What the binary hands over.
pub struct RcHost {
    pub config_home: PathBuf,
    pub store: BackgroundStore,
    pub projects_root: PathBuf,
    /// The running `rebon` executable workers are started from. Never a
    /// bare name looked up on `PATH`.
    pub rebon_exe: PathBuf,
    /// `--provider`, `--model`, `--permission-mode` and friends, applied
    /// to every session this runner opens.
    pub runtime: BackgroundRuntimeFields,
    pub launch_gate: LaunchGate,
    pub permission_projection: PermissionProjection,
    pub configured_projects: ProjectSource,
}

/// Run a `rebon rc` subcommand.
pub async fn run(command: RcCommand, host: RcHost) -> anyhow::Result<()> {
    let dir = RcDir::new(&host.config_home);
    match command {
        RcCommand::Login(args) => {
            let token = read_token()?;
            let credential = match args.token_kind {
                TokenKind::Bootstrap => LoginCredential::Bootstrap(token),
                TokenKind::Access => LoginCredential::AccessToken(token),
                TokenKind::Refresh => LoginCredential::RefreshToken(token),
            };
            let label = args.label.or_else(|| Some(machine_name()));
            let stored = login::login(&dir, &args.server, credential, label).await?;
            println!("logged in · {}", stored.server);
            if !stored.device_id.is_empty() {
                println!("  device  {}", stored.device_id);
                println!("  account {}", stored.account_id);
            }
            println!("  credentials {}", dir.credentials_path().display());
            println!("next: rebon rc serve --project <dir>");
            Ok(())
        }
        RcCommand::Status(args) => {
            let cwd =
                std::env::current_dir().context("rebon rc status needs a working directory")?;
            let projects = (host.configured_projects)()
                .and_then(|configured| resolve_projects(configured, &[], &cwd));
            let status = login::status(&dir, &host.store, projects)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print!("{}", login::render_status(&status));
            }
            Ok(())
        }
        RcCommand::Serve(args) => serve_command(dir, host, args).await,
    }
}

async fn serve_command(dir: RcDir, host: RcHost, args: ServeArgs) -> anyhow::Result<()> {
    let credentials = login::require_credentials(&dir)?;
    // Fail on the unattended-launch policy now, not on the first work item.
    (host.launch_gate)(&host.runtime)?;
    let start_dir = std::env::current_dir().context("rebon rc serve needs a working directory")?;
    let client = HttpBridgeApiClient::new(
        HttpClientConfig::new(credentials.server.clone()),
        DeviceCredentials::new(String::new(), credentials.refresh_token.clone()),
    )?;
    client
        .refresh_now()
        .await
        .context("the RC server refused this device; run `rebon rc login` again")?;
    let session_host = LocalSessionHost::new(
        SessionHostClient::new(
            host.store.clone(),
            host.projects_root.clone(),
            host.rebon_exe.clone(),
        ),
        host.runtime.clone(),
        Arc::clone(&host.launch_gate),
        HostTiming::default(),
    );
    let context = WorkContext {
        api: Arc::new(HttpEnvironmentApi(client)),
        streams: Arc::new(WsConnector),
        host: Arc::new(session_host),
        ledger: Ledger::new(dir.clone()),
        projection: host.permission_projection,
        environment_id: String::new(),
        timing: WorkTiming::default(),
        // Remote answers affect one call, never a standing rule.
        policy: RemotePermissionPolicy::one_shot(),
        pid: std::process::id(),
    };
    let config = ServeConfig {
        server: credentials.server,
        machine_name: args.machine_name.unwrap_or_else(machine_name),
        max_sessions: args.max_sessions.max(1),
        configured: host.configured_projects,
        project_flags: args.projects,
        start_dir,
        timing: ServeTiming::default(),
    };
    eprintln!(
        "rebon rc: sessions served here are visible to {} in plain text",
        config.server
    );
    serve(&dir, context, config, async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "rebon rc: cannot listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
        eprintln!("rebon rc: shutting down");
    })
    .await
}

fn read_token() -> anyhow::Result<String> {
    if let Some(token) = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|token| !token.trim().is_empty())
    {
        return Ok(token.trim().to_string());
    }
    eprintln!("paste the token and press Enter (or set {TOKEN_ENV}):");
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read the token from stdin")?;
    let token = line.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("no token was given");
    }
    Ok(token)
}

/// This machine's name, for the server's list.
pub fn machine_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|name| name.trim().to_string())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "rebon".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(subcommand)]
        command: RcCommand,
    }

    #[test]
    fn the_subcommands_parse() {
        let login = Cli::try_parse_from(["rc", "login", "--server", "https://rc"]).unwrap();
        assert_eq!(
            login.command,
            RcCommand::Login(LoginArgs {
                server: "https://rc".into(),
                token_kind: TokenKind::Bootstrap,
                label: None,
            })
        );
        let refresh = Cli::try_parse_from([
            "rc",
            "login",
            "--server",
            "https://rc",
            "--token-kind",
            "refresh",
            "--label",
            "laptop",
        ])
        .unwrap();
        assert!(matches!(
            refresh.command,
            RcCommand::Login(LoginArgs {
                token_kind: TokenKind::Refresh,
                ..
            })
        ));
        let serve = Cli::try_parse_from([
            "rc",
            "serve",
            "--project",
            "a",
            "--project",
            "b",
            "--max-sessions",
            "2",
        ])
        .unwrap();
        assert_eq!(
            serve.command,
            RcCommand::Serve(ServeArgs {
                projects: vec!["a".into(), "b".into()],
                max_sessions: 2,
                machine_name: None,
            })
        );
        let status = Cli::try_parse_from(["rc", "status", "--json"]).unwrap();
        assert_eq!(status.command, RcCommand::Status(StatusArgs { json: true }));
        // The token is never an argument.
        assert!(Cli::try_parse_from(["rc", "login", "--server", "s", "--token", "t"]).is_err());
        assert!(Cli::try_parse_from(["rc", "login"]).is_err());
    }

    #[test]
    fn a_machine_has_a_name() {
        assert!(!machine_name().is_empty());
    }
}
