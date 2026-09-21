//! `rebon remote …`: the subcommands that configure a host and set up its
//! server build.
//!
//! These are clap subcommands, parsed before a kernel exists, so they are
//! not commands on a seat and cannot be — `rebon remote list` has to answer
//! without booting a plugin registry, and `plugins.remote.enabled` is a
//! question only a booted one can be asked. What lives here is
//! everything after the match arm: the binary names a branch and prints
//! nothing of its own, and the flags, the probe-and-install sequence and the
//! report layout live next to the store they describe.
//!
//! The session-time half of the plugin is [`crate::agents`], and it *is*
//! behind the switch: a disabled plugin declares no hosts, so `/agent` and
//! `/context` show none, even while `rebon remote list` still prints them.
//! That asymmetry is deliberate. Turning the feature off should stop Rebon
//! from *running* on a remote; it should not make the user's configuration
//! unreadable.

use std::path::PathBuf;

use clap::Subcommand;

use crate::agents::{config_dir, load_store, resolve, server_version};
use crate::remote::{
    InstallOptions, InstallOutcome, InstallStrategy, ProbeReport, RemoteHost, RemotePlatform,
    SshOptions, Transport,
};

/// `rebon remote`.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum RemoteCommand {
    /// Register a remote host and set up its server build.
    Add {
        /// Name used by `rebon --remote <name>` and `/agent`.
        #[arg(value_name = "NAME")]
        name: String,
        /// ssh destination: `user@host`, `host:port`, an
        /// `~/.ssh/config` alias, or `ssh://user@host:port/path`.
        #[arg(value_name = "TARGET")]
        target: String,
        /// Default project directory on the remote.
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// How the server build gets there: fetch, push, npm, system.
        #[arg(long, value_parser = parse_install_strategy)]
        install: Option<InstallStrategy>,
        /// How the protocol travels: stdio (default) or forward.
        #[arg(long, value_parser = parse_transport)]
        transport: Option<Transport>,
        /// ssh identity file (`-i`).
        #[arg(long = "identity", value_name = "FILE")]
        identity_file: Option<String>,
        /// Alternate ssh_config (`-F`).
        #[arg(long = "ssh-config", value_name = "FILE")]
        ssh_config: Option<String>,
        /// Jump host / bastion (`-J`).
        #[arg(long = "jump", value_name = "HOST")]
        jump_host: Option<String>,
        /// Where server builds are installed on the remote.
        #[arg(long = "server-dir", value_name = "DIR")]
        server_dir: Option<String>,
        /// Send this machine's provider credentials to the remote
        /// process environment for the life of each connection.
        #[arg(long = "forward-credentials")]
        forward_credentials: bool,
        /// Replace an existing remote of the same name.
        #[arg(long)]
        force: bool,
        /// Record the host without probing or installing.
        #[arg(long = "no-setup")]
        no_setup: bool,
    },
    /// List configured remotes.
    List,
    /// Show one remote's full configuration.
    Show {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Forget a remote. Does not touch the server build.
    Remove {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Install or update the server build on a remote.
    Install {
        #[arg(value_name = "NAME")]
        name: String,
        /// Reinstall even when this version is already there.
        #[arg(long)]
        force: bool,
        /// Package to push, for `--install push` or a cross-platform
        /// target. A `.tgz` in the shape `@rebon/cli-<os>-<cpu>` has.
        #[arg(long, value_name = "PATH")]
        package: Option<PathBuf>,
        /// npm registry to fetch from.
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
    },
    /// Remove the server build from a remote.
    Uninstall {
        #[arg(value_name = "NAME")]
        name: String,
        /// Remove every installed version, not just this one.
        #[arg(long)]
        all: bool,
    },
    /// Probe a remote and report what is there.
    Status {
        #[arg(value_name = "NAME")]
        name: String,
    },
}

fn parse_install_strategy(raw: &str) -> Result<InstallStrategy, String> {
    InstallStrategy::parse(raw)
        .ok_or_else(|| format!("expected one of fetch, push, npm, system; got `{raw}`"))
}

fn parse_transport(raw: &str) -> Result<Transport, String> {
    Transport::parse(raw).ok_or_else(|| format!("expected one of stdio, forward; got `{raw}`"))
}

/// Run one `rebon remote …`.
pub fn run_remote_command(command: RemoteCommand) -> anyhow::Result<()> {
    match command {
        RemoteCommand::Add {
            name,
            target,
            path,
            install,
            transport,
            identity_file,
            ssh_config,
            jump_host,
            server_dir,
            forward_credentials,
            force,
            no_setup,
        } => add(
            &name,
            &target,
            &AddOptions {
                path,
                install,
                transport,
                identity_file,
                ssh_config,
                jump_host,
                server_dir,
                forward_credentials,
                force,
                no_setup,
            },
        ),
        RemoteCommand::List => list(),
        RemoteCommand::Show { name } => show(&name),
        RemoteCommand::Remove { name } => remove(&name),
        RemoteCommand::Install {
            name,
            force,
            package,
            registry,
        } => install(
            &name,
            &InstallOptions {
                force,
                package,
                registry,
            },
        ),
        RemoteCommand::Uninstall { name, all } => uninstall(&name, all),
        RemoteCommand::Status { name } => status(&name),
    }
}

/// ssh options for a one-shot management command.
///
/// `interactive` lets ssh prompt for a passphrase or a host key, which
/// is right here and wrong for the session transport — see
/// [`crate::remote::ssh`].
fn management_options(host: &RemoteHost, interactive: bool) -> SshOptions {
    let mut options = host.ssh_options();
    if let Some(dir) = crate::remote::control_dir(&config_dir()) {
        options.control_dir = Some(dir);
    } else {
        options.multiplex = false;
    }
    if interactive {
        options = options.interactive();
    }
    options
}

/// Arguments `rebon remote add` accepts beyond the name and target.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct AddOptions {
    pub path: Option<String>,
    pub install: Option<InstallStrategy>,
    pub transport: Option<Transport>,
    pub identity_file: Option<String>,
    pub ssh_config: Option<String>,
    pub jump_host: Option<String>,
    pub server_dir: Option<String>,
    pub forward_credentials: bool,
    pub force: bool,
    /// Skip the probe-and-install that otherwise runs on add.
    pub no_setup: bool,
}

fn add(name: &str, target: &str, options: &AddOptions) -> anyhow::Result<()> {
    let mut host = RemoteHost::from_target(name, target)?;
    if let Some(path) = &options.path {
        host.path = Some(path.clone());
    }
    if let Some(install) = options.install {
        host.install = install;
    }
    if let Some(transport) = options.transport {
        host.transport = transport;
    }
    host.identity_file = options.identity_file.clone();
    host.ssh_config = options.ssh_config.clone();
    host.jump_host = options.jump_host.clone();
    host.server_dir = options.server_dir.clone();
    host.forward_credentials = options.forward_credentials;

    let mut store = load_store()?;
    store.insert(host.clone(), options.force)?;
    store.save(&config_dir())?;
    println!("added remote `{}` → {}", host.name, host.summary());

    if options.no_setup {
        println!(
            "run `rebon remote install {}` to set up the server",
            host.name
        );
        return Ok(());
    }
    // Probing on add is what turns "I typed the wrong hostname" into
    // an error now rather than at the start of the first session.
    match setup(&host, &InstallOptions::default()) {
        Ok(()) => Ok(()),
        Err(err) => {
            // The record is kept: a host that is merely unreachable
            // right now is still one the user meant to configure, and
            // making them retype it would be worse than a warning.
            println!("remote `{}` is saved but not ready: {err}", host.name);
            println!(
                "fix the connection, then run `rebon remote install {}`",
                host.name
            );
            Ok(())
        }
    }
}

/// Probe, install, and record what was found.
fn setup(host: &RemoteHost, options: &InstallOptions) -> anyhow::Result<()> {
    let ssh = management_options(host, true);
    let version = server_version();
    let report = crate::remote::probe(host, &ssh, version)?;
    let platform = report.platform()?;
    println!("  {} is {}", host.name, platform);

    let install = crate::remote::install(host, &ssh, version, &report, options)?;
    match install.outcome {
        InstallOutcome::Installed => {
            println!("  installed rebon {version} from {}", install.source)
        }
        InstallOutcome::AlreadyPresent => {
            println!("  rebon {version} is already installed")
        }
        InstallOutcome::Unknown => unreachable!("install() rejects an unknown outcome"),
    }

    remember(host, Some(platform), Some(version))?;
    Ok(())
}

/// Write back what a probe learned, leaving everything else alone.
fn remember(
    host: &RemoteHost,
    platform: Option<RemotePlatform>,
    version: Option<&str>,
) -> anyhow::Result<()> {
    let mut store = load_store()?;
    let Some(entry) = store.get_mut(&host.name) else {
        return Ok(());
    };
    if let Some(platform) = platform {
        entry.platform = Some(platform.to_string());
    }
    if let Some(version) = version {
        entry.installed_version = Some(version.to_string());
    }
    store.save(&config_dir())?;
    Ok(())
}

fn list() -> anyhow::Result<()> {
    let store = load_store()?;
    if store.hosts.is_empty() {
        println!("No remotes configured. Add one with `rebon remote add <name> <user@host>`.");
        return Ok(());
    }
    for host in &store.hosts {
        println!(
            "{name}  {target}  {transport}/{install}{server}",
            name = host.name,
            target = host.summary(),
            transport = host.transport.as_str(),
            install = host.install.as_str(),
            server = host
                .installed_version
                .as_deref()
                .map(|v| format!("  server {v}"))
                .unwrap_or_else(|| "  not installed".to_string()),
        );
    }
    Ok(())
}

fn show(name: &str) -> anyhow::Result<()> {
    let host = resolve(name)?;
    println!("name              {}", host.name);
    println!("target            {}", host.target());
    println!(
        "project path      {}",
        host.path.as_deref().unwrap_or("(remote home directory)")
    );
    println!("transport         {}", host.transport.as_str());
    println!("install strategy  {}", host.install.as_str());
    println!(
        "server directory  {}",
        host.server_dir.as_deref().unwrap_or("$HOME/.rebon/server")
    );
    println!(
        "platform          {}",
        host.platform.as_deref().unwrap_or("(not probed)")
    );
    println!(
        "server version    {}",
        host.installed_version.as_deref().unwrap_or("(none)")
    );
    println!(
        "forward creds     {}",
        if host.forward_credentials {
            "yes"
        } else {
            "no"
        }
    );
    if let Some(identity) = &host.identity_file {
        println!("identity file     {identity}");
    }
    if let Some(jump) = &host.jump_host {
        println!("jump host         {jump}");
    }
    println!("agent id          {}", crate::remote::agent_id(&host.name));
    Ok(())
}

fn remove(name: &str) -> anyhow::Result<()> {
    let mut store = load_store()?;
    let removed = store.remove(name)?;
    store.save(&config_dir())?;
    println!("removed remote `{}`", removed.name);
    println!(
        "the server build on {} was left in place; `rebon remote uninstall` removes it",
        removed.target()
    );
    Ok(())
}

fn install(name: &str, options: &InstallOptions) -> anyhow::Result<()> {
    let host = resolve(name)?;
    setup(&host, options)
}

fn uninstall(name: &str, all: bool) -> anyhow::Result<()> {
    let host = resolve(name)?;
    let ssh = management_options(&host, true);
    let version = (!all).then(server_version);
    crate::remote::uninstall(&host, &ssh, version)?;
    match version {
        Some(version) => println!("removed rebon {version} from `{}`", host.name),
        None => println!("removed every server build from `{}`", host.name),
    }
    if all {
        remember(&host, None, None)?;
    }
    Ok(())
}

fn status(name: &str) -> anyhow::Result<()> {
    let host = resolve(name)?;
    let ssh = management_options(&host, false);
    let report = crate::remote::probe(&host, &ssh, server_version())?;
    print_report(&host, &report);
    Ok(())
}

fn print_report(host: &RemoteHost, report: &ProbeReport) {
    println!("host              {}", host.target());
    match report.platform() {
        Ok(platform) => println!("platform          {platform}"),
        Err(err) => println!("platform          unsupported ({err})"),
    }
    println!(
        "remote home       {}",
        report.home.as_deref().unwrap_or("(unknown)")
    );
    println!(
        "server {}      {}",
        server_version(),
        report
            .server_version
            .as_deref()
            .unwrap_or("not installed — run `rebon remote install`")
    );
    println!(
        "rebon on PATH     {}",
        report.path_rebon.as_deref().unwrap_or("(none)")
    );
    println!(
        "downloader        {}",
        report
            .downloader()
            .unwrap_or("none — `fetch` installs need curl or wget")
    );
    let mut tools = report.tools.clone();
    tools.sort();
    println!("remote tools      {}", tools.join(" "));
}
