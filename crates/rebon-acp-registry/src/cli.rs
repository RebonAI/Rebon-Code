//! `rebon agent …`: the ACP agents `config.json` declares, and the public
//! registry they can be installed from. `--json` gives each answer in a
//! form the desktop app reads.

use std::path::Path;

use anyhow::Context;
use serde_json::{json, Value};

use crate::fetch::{self, Fetched};
use crate::{install, Registry, RegistryAgent};

#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum AgentCommand {
    /// The ACP agents config.json declares: what `/backend` can switch to.
    List(ListArgs),
    /// The agents the public ACP registry lists: whether each can be
    /// installed here and how, and which are installed.
    Registry(RegistryArgs),
    /// Install an agent from the ACP registry as an `acpAgents` entry.
    ///
    /// A binary agent is downloaded, checked and unpacked under
    /// `<config home>/acp-agents/`; an npm or Python one is started through
    /// npx or uvx, which must be on PATH. Adding an installed agent again
    /// updates it. An entry written by hand is never replaced. Google
    /// Antigravity and ZCode are not installed this way: their providers
    /// allow only their own CLI, so they run in a terminal.
    Add(AddArgs),
    /// Remove an agent from config.json, with the files a registry install
    /// downloaded for it.
    Remove(RemoveArgs),
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct RegistryArgs {
    #[arg(long)]
    pub json: bool,
    /// Read the registry again rather than the copy kept from the last hour.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct AddArgs {
    /// The registry's id for the agent (pi-acp, goose, opencode, …), as
    /// `rebon agent registry` lists it.
    pub agent: String,
    /// The id to give it in config.json; by default the registry's without
    /// `-acp` (`pi-acp` becomes `pi`).
    #[arg(long = "as", value_name = "ID")]
    pub id: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args, PartialEq, Eq)]
pub struct RemoveArgs {
    /// The agent's id in config.json.
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

/// Runs a `rebon agent` subcommand against the user's config home.
pub async fn run(command: AgentCommand) -> anyhow::Result<()> {
    let config_dir = rebon_config::config_home_dir();
    match command {
        AgentCommand::List(args) => list(&config_dir, args.json),
        AgentCommand::Registry(args) => {
            let fetched = load(&config_dir, args.refresh).await?;
            let rows = registry_rows(&config_dir, &fetched.registry)?;
            if args.json {
                print_json(&json!({
                    "registryVersion": fetched.registry.version,
                    "fetchedAtMs": fetched.fetched_at_ms,
                    "fromCache": fetched.from_cache,
                    "stale": fetched.stale_because,
                    "platform": crate::current_platform(),
                    "agents": rows,
                }))
            } else {
                if let Some(why) = &fetched.stale_because {
                    eprintln!(
                        "the registry could not be read ({why}); showing the copy kept from before"
                    );
                }
                for row in &rows {
                    let state = if let Some(installed) = row["installed"].as_object() {
                        let update = if row["updateAvailable"] == true {
                            ", update available"
                        } else {
                            ""
                        };
                        format!(
                            "installed as `{}` ({}{update})",
                            installed["id"].as_str().unwrap_or(""),
                            installed["version"].as_str().unwrap_or("")
                        )
                    } else if let Some(method) = row["method"].as_str() {
                        format!("can install ({method})")
                    } else {
                        format!(
                            "cannot install: {}",
                            row["unavailableText"].as_str().unwrap_or("")
                        )
                    };
                    println!(
                        "{:<22} {:<24} {:<12} {state}",
                        row["id"].as_str().unwrap_or(""),
                        row["name"].as_str().unwrap_or(""),
                        row["version"].as_str().unwrap_or(""),
                    );
                }
                Ok(())
            }
        }
        AgentCommand::Add(args) => {
            let fetched = load(&config_dir, false).await?;
            let agent = find(&fetched.registry, &args.agent)?;
            let client = fetch::client()?;
            let installed =
                install::install(&client, &config_dir, agent, args.id.as_deref()).await?;
            if args.json {
                print_json(&json!({
                    "id": installed.agent.id,
                    "name": installed.agent.label(),
                    "method": installed.method,
                    "command": installed.agent.command,
                    "args": installed.agent.args,
                    "registry": installed.agent.registry,
                    "replacedVersion": installed.replaced.as_ref().and_then(|old| old.registry.as_ref()).map(|old| &old.version),
                    "unpackedIn": installed.unpacked_in,
                }))
            } else {
                let verb = if installed.replaced.is_some() {
                    "updated"
                } else {
                    "installed"
                };
                println!(
                    "{verb} {} as `{}` ({}); switch a session to it with /backend {}",
                    installed.agent.label(),
                    installed.agent.id,
                    installed.method,
                    installed.agent.id
                );
                Ok(())
            }
        }
        AgentCommand::Remove(args) => {
            let removed = install::remove(&config_dir, &args.id)?;
            if args.json {
                return print_json(&json!({
                    "removed": removed.as_ref().map(|removed| &removed.agent.id),
                    "deleted": removed.as_ref().and_then(|removed| removed.deleted.as_ref()),
                }));
            }
            match removed {
                Some(removed) => {
                    println!("removed `{}`", removed.agent.id);
                    if let Some(dir) = removed.deleted {
                        println!("deleted {}", dir.display());
                    }
                    Ok(())
                }
                None => anyhow::bail!("config.json declares no agent `{}`", args.id),
            }
        }
    }
}

async fn load(config_dir: &Path, refresh: bool) -> anyhow::Result<Fetched> {
    let client = fetch::client()?;
    fetch::load(&client, config_dir, fetch::DEFAULT_MAX_AGE, refresh).await
}

fn find<'a>(registry: &'a Registry, id: &str) -> anyhow::Result<&'a RegistryAgent> {
    registry.agent(id).with_context(|| {
        format!(
            "the ACP registry lists no agent `{id}`; `rebon agent registry` shows the ones it does"
        )
    })
}

fn list(config_dir: &Path, as_json: bool) -> anyhow::Result<()> {
    let agents = rebon_config::load_acp_agents_in(config_dir)?;
    let active = rebon_config::active_acp_agent_in(config_dir);
    if as_json {
        let rows: Vec<Value> = agents
            .iter()
            .map(|agent| {
                json!({
                    "id": agent.id,
                    "name": agent.label(),
                    "command": agent.command,
                    "args": agent.args,
                    "registry": agent.registry,
                    "active": active.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(&agent.id)),
                })
            })
            .collect();
        return print_json(&json!({ "agents": rows, "active": active }));
    }
    if agents.is_empty() {
        println!("no ACP agents in config.json; `rebon agent registry` lists the ones you can add");
    }
    for agent in &agents {
        let origin = match &agent.registry {
            Some(source) => format!("registry {} {}", source.id, source.version),
            None => "by hand".to_string(),
        };
        let marker = if active
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(&agent.id))
        {
            " (new sessions start on it)"
        } else {
            ""
        };
        println!("{:<16} {:<24} {origin}{marker}", agent.id, agent.label());
    }
    Ok(())
}

/// One row per registry agent: what it is, how it would install here or
/// why it cannot, and the entry it is installed as.
fn registry_rows(config_dir: &Path, registry: &Registry) -> anyhow::Result<Vec<Value>> {
    let declared = rebon_config::load_acp_agents_in(config_dir)?;
    let platform = crate::current_platform();
    Ok(registry
        .agents
        .iter()
        .map(|agent| {
            let installed = declared.iter().find(|entry| {
                entry
                    .registry
                    .as_ref()
                    .is_some_and(|source| source.id.eq_ignore_ascii_case(&agent.id))
            });
            let chosen = crate::choose(agent, platform, crate::on_path);
            json!({
                "id": agent.id,
                "name": agent.name,
                "version": agent.version,
                "description": agent.description,
                "repository": agent.repository,
                "website": agent.website,
                "icon": agent.icon,
                "defaultId": crate::default_id(&agent.id),
                "terminalOnly": crate::terminal_only(&agent.id).is_some(),
                "method": chosen.as_ref().ok().map(crate::Method::kind),
                "unavailable": chosen.as_ref().err(),
                "unavailableText": chosen.as_ref().err().map(ToString::to_string),
                "installed": installed.map(|entry| json!({
                    "id": entry.id,
                    "version": entry.registry.as_ref().map(|source| &source.version),
                })),
                "updateAvailable": installed
                    .and_then(|entry| entry.registry.as_ref())
                    .is_some_and(|source| source.version != agent.version),
            })
        })
        .collect())
}

fn print_json(value: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: AgentCommand,
    }

    fn parse(args: &[&str]) -> Result<AgentCommand, clap::Error> {
        Cli::try_parse_from(std::iter::once("agent").chain(args.iter().copied()))
            .map(|cli| cli.command)
    }

    #[test]
    fn the_commands_parse() {
        assert!(parse(&["list", "--json"]).is_ok());
        assert!(parse(&["registry", "--refresh"]).is_ok());
        assert_eq!(
            parse(&["add", "pi-acp", "--as", "pi"]).unwrap(),
            AgentCommand::Add(AddArgs {
                agent: "pi-acp".into(),
                id: Some("pi".into()),
                json: false,
            })
        );
        assert!(parse(&["remove", "pi", "--json"]).is_ok());
        assert!(parse(&["add"]).is_err());
    }

    #[test]
    fn rows_say_what_is_installed_what_can_be_and_what_runs_only_in_a_terminal() {
        let config = tempfile::tempdir().unwrap();
        let registry = Registry::parse(&crate::tests::sample()).unwrap();
        let mut pi = rebon_config::AcpAgentConfig {
            id: "pi".into(),
            display_name: None,
            command: "npx".into(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            inject_fs_tools: None,
            session_meta: None,
            registry: Some(rebon_config::AcpRegistrySource {
                id: "pi-acp".into(),
                version: "0.0.34".into(),
            }),
        };
        rebon_config::upsert_acp_agent_in(config.path(), &pi).unwrap();

        let rows = registry_rows(config.path(), &registry).unwrap();
        let row = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap().clone();
        assert_eq!(row("pi-acp")["installed"]["id"], "pi");
        assert_eq!(row("pi-acp")["updateAvailable"], true);
        assert_eq!(row("pi-acp")["defaultId"], "pi");
        assert_eq!(row("antigravity-acp")["terminalOnly"], true);
        assert_eq!(
            row("antigravity-acp")["unavailable"]["reason"],
            "terminalOnly"
        );
        assert!(row("antigravity-acp")["method"].is_null());
        assert_eq!(row("goose")["installed"], Value::Null);

        pi.registry.as_mut().unwrap().version = "0.0.35".into();
        rebon_config::upsert_acp_agent_in(config.path(), &pi).unwrap();
        let rows = registry_rows(config.path(), &registry).unwrap();
        let pi_row = rows.iter().find(|row| row["id"] == "pi-acp").unwrap();
        assert_eq!(pi_row["updateAvailable"], false);
    }
}
