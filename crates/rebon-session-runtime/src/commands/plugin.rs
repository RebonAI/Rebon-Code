//! `/plugin` -- what is installed, and installing or removing one.
//!
//! Listing, installing from a path or a name, verifying a digest, enabling,
//! disabling and removing all run against the plugin store on disk. The one
//! thing the command reads from the session is its working directory, which
//! decides what "project scope" means, so this belongs beside the session
//! rather than inside the view that happens to type it.
//!
//! The marketplaces are here too: `/plugin marketplace add|list|update|remove`,
//! `/plugin browse`, and `/plugin install plugin@marketplace`. Adding,
//! updating and installing from one go to the network
//! ([`plugin_command_needs_network`]), so a front end with a loop to keep
//! drawing runs those through [`handle_plugin_command_in`] off it.

use std::path::PathBuf;

use crate::commands::{command_args, name_ends_here, tokenize_command_args};
use crate::EngineSession;
use rebon_slash_commands::strip_command_prefix;

pub struct PluginCommandResult {
    pub text: String,
    pub is_err: bool,
}

impl PluginCommandResult {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_err: false,
        }
    }

    fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_err: true,
        }
    }
}

/// Returns `Some(())` when the text names `/plugin` (or `/plugins`).
pub fn parse_plugin_command(text: &str) -> Option<()> {
    name_ends_here(strip_command_prefix(text, "plugin")?).then_some(())
}

/// What a `/plugin` command needs from where it was typed: no session, so it
/// can run on another thread.
#[derive(Clone, Debug)]
pub struct PluginCommandContext {
    pub cwd: PathBuf,
    pub plugin_dirs: Vec<PathBuf>,
    pub config_home: PathBuf,
}

impl PluginCommandContext {
    pub fn of(session: &EngineSession) -> Self {
        Self {
            cwd: PathBuf::from(&session.cwd),
            plugin_dirs: session
                .startup
                .plugin_dirs
                .iter()
                .map(PathBuf::from)
                .collect(),
            config_home: crate::rebon_config::config_home_dir(),
        }
    }
}

/// Whether a `/plugin` command goes to the network: adding or updating a
/// marketplace, or installing a plugin from one.
pub fn plugin_command_needs_network(text: &str, cwd: &std::path::Path) -> bool {
    let Ok(tokens) = tokenize_plugin_args(command_args(text, "plugin")) else {
        return false;
    };
    match tokens
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["marketplace", "add" | "update", ..] => true,
        ["install", rest @ ..] => rest
            .iter()
            .filter(|token| !token.starts_with("--"))
            .any(|token| crate::plugin::marketplace::is_marketplace_spec(token, cwd)),
        _ => false,
    }
}

pub fn handle_plugin_command(text: &str, session: &EngineSession) -> PluginCommandResult {
    handle_plugin_command_in(text, &PluginCommandContext::of(session))
}

pub fn handle_plugin_command_in(text: &str, context: &PluginCommandContext) -> PluginCommandResult {
    let args = command_args(text, "plugin");
    let mut tokens = match tokenize_plugin_args(args) {
        Ok(tokens) => tokens,
        Err(err) => return PluginCommandResult::err(err),
    };
    if tokens.is_empty() {
        return PluginCommandResult::ok(plugin_usage_text());
    }

    let replace_source = tokens.iter().any(|token| token == "--replace-source");
    if replace_source && tokens[0] != "install" {
        return PluginCommandResult::err("--replace-source is only supported by /plugin install");
    }
    tokens.retain(|token| token != "--replace-source");
    let cwd = context.cwd.clone();
    let store = crate::plugin::PluginStore::new(context.config_home.clone(), cwd.clone());
    let installer =
        crate::plugin::PluginInstaller::new(store, cwd.clone(), context.plugin_dirs.clone())
            .with_replace_source(replace_source);
    let marketplaces = crate::plugin::marketplace::MarketplaceManager::new(
        context.config_home.clone(),
        cwd.clone(),
    )
    .with_replace_source(replace_source);
    match tokens[0].as_str() {
        "browse" | "discover" => match marketplaces.browse() {
            Ok(catalog) => PluginCommandResult::ok(crate::plugin::marketplace::format_catalog(
                &catalog,
                tokens.get(1).map(String::as_str),
            )),
            Err(err) => PluginCommandResult::err(format!("{err:#}")),
        },
        "marketplace" | "marketplaces" => marketplace_command(&marketplaces, &tokens[1..]),
        "install"
            if tokens[1..]
                .iter()
                .filter(|token| !token.starts_with("--"))
                .any(|token| crate::plugin::marketplace::is_marketplace_spec(token, &cwd)) =>
        {
            let (scope, rest) = parse_plugin_scope_arg(&tokens[1..]);
            let scope = match scope {
                Ok(scope) => scope,
                Err(err) => return PluginCommandResult::err(err),
            };
            let spec = rest.first().cloned().unwrap_or_default();
            match marketplaces.install(&spec, scope) {
                Ok(install) => PluginCommandResult::ok(crate::plugin::marketplace::format_install(
                    "installed",
                    &install,
                )),
                Err(err) => PluginCommandResult::err(format!("{err:#}")),
            }
        }
        "uninstall" | "remove" | "rm"
            if tokens.get(1).is_some_and(|name| {
                marketplaces
                    .installs()
                    .is_ok_and(|installs| installs.by_id.contains_key(name))
            }) =>
        {
            match marketplaces.uninstall(&tokens[1]) {
                Ok(install) => PluginCommandResult::ok(crate::plugin::marketplace::format_install(
                    "uninstalled",
                    &install,
                )),
                Err(err) => PluginCommandResult::err(format!("{err:#}")),
            }
        }
        "install" => {
            let (scope, rest) = parse_plugin_scope_arg(&tokens[1..]);
            let scope = match scope {
                Ok(scope) => scope,
                Err(err) => return PluginCommandResult::err(err),
            };
            let (sha256, rest) = match parse_plugin_sha256_arg(&rest) {
                Ok(parsed) => parsed,
                Err(err) => return PluginCommandResult::err(err),
            };
            let Some(source) = rest.first() else {
                return PluginCommandResult::err(
                    "Usage: /plugin install <path|archive|name|rust-lsp> [--scope user|project] [--sha256 <hex>] [--replace-source]",
                );
            };
            match installer.install(source, scope, sha256.as_deref()) {
                Ok(record) => PluginCommandResult::ok(crate::plugin::format_plugin_result(
                    "installed",
                    scope,
                    &record,
                )),
                Err(err) => PluginCommandResult::err(format!("{err:#}")),
            }
        }
        "enable" => plugin_mutate_name(
            &installer,
            &tokens[1..],
            "enabled",
            |installer, name, scope| installer.enable(name, scope),
        ),
        "disable" => plugin_mutate_name(
            &installer,
            &tokens[1..],
            "disabled",
            |installer, name, scope| installer.disable(name, scope),
        ),
        "uninstall" | "remove" | "rm" => {
            let (scope, rest) = parse_plugin_scope_arg(&tokens[1..]);
            let scope = match scope {
                Ok(scope) => scope,
                Err(err) => return PluginCommandResult::err(err),
            };
            let Some(name) = rest.first() else {
                return PluginCommandResult::err(
                    "Usage: /plugin uninstall <name> [--scope user|project]",
                );
            };
            match installer.uninstall(name, scope) {
                Ok(Some(record)) => PluginCommandResult::ok(crate::plugin::format_plugin_result(
                    "uninstalled",
                    scope,
                    &record,
                )),
                Ok(None) => PluginCommandResult::ok(format!(
                    "plugin `{name}` is not installed in {} scope",
                    scope.as_str()
                )),
                Err(err) => PluginCommandResult::err(err.to_string()),
            }
        }
        "list" | "ls" => {
            let scope = match parse_optional_scope(&tokens[1..]) {
                Ok(scope) => scope,
                Err(err) => return PluginCommandResult::err(err),
            };
            match installer.list(scope) {
                Ok(records) => PluginCommandResult::ok(format!(
                    "{}\n\n{}",
                    format_plugin_records(records),
                    crate::commands::kernel::kernel_plugins_summary()
                )),
                Err(err) => PluginCommandResult::err(err.to_string()),
            }
        }
        "status" => {
            let (scope, rest) = match parse_optional_scope_and_name(&tokens[1..]) {
                Ok(parsed) => parsed,
                Err(err) => return PluginCommandResult::err(err),
            };
            match installer.status(rest.first().map(String::as_str), scope) {
                Ok(records) => PluginCommandResult::ok(format_plugin_records(records)),
                Err(err) => PluginCommandResult::err(err.to_string()),
            }
        }
        "verify" => {
            let (scope, rest) = match parse_optional_scope_and_name(&tokens[1..]) {
                Ok(parsed) => parsed,
                Err(err) => return PluginCommandResult::err(err),
            };
            match installer.verify(rest.first().map(String::as_str), scope) {
                Ok(report) => {
                    if report.is_empty() {
                        return PluginCommandResult::ok("No plugins installed.".to_string());
                    }
                    let lines: Vec<String> = report
                        .iter()
                        .map(crate::plugin::PluginVerification::describe)
                        .collect();
                    // Drift is a finding, not a command failure: the report is
                    // the point, so it renders either way.
                    if report.iter().any(|entry| entry.is_problem()) {
                        PluginCommandResult::err(lines.join("\n"))
                    } else {
                        PluginCommandResult::ok(lines.join("\n"))
                    }
                }
                Err(err) => PluginCommandResult::err(err.to_string()),
            }
        }
        _ => PluginCommandResult::ok(plugin_usage_text()),
    }
}

/// `/plugin marketplace add|list|update|remove`.
fn marketplace_command(
    marketplaces: &crate::plugin::marketplace::MarketplaceManager,
    args: &[String],
) -> PluginCommandResult {
    const USAGE: &str = "Usage: /plugin marketplace add <owner/repo|git url|url|path> | list | update [name] | remove <name>";
    match args.first().map(String::as_str) {
        Some("add") => {
            let Some(source) = args.get(1) else {
                return PluginCommandResult::err(USAGE);
            };
            match marketplaces.add(source) {
                Ok(view) => PluginCommandResult::ok(format!(
                    "added marketplace {} ({} plugins) from {}",
                    view.name, view.plugins, view.source
                )),
                Err(err) => PluginCommandResult::err(format!("{err:#}")),
            }
        }
        Some("list" | "ls") | None => match marketplaces.marketplaces() {
            Ok(views) => {
                PluginCommandResult::ok(crate::plugin::marketplace::format_marketplaces(&views))
            }
            Err(err) => PluginCommandResult::err(format!("{err:#}")),
        },
        Some("update") => match marketplaces.update(args.get(1).map(String::as_str)) {
            Ok(outcomes) => {
                let failed = outcomes.iter().any(|(_, outcome)| outcome.is_err());
                let lines: Vec<String> = outcomes
                    .into_iter()
                    .map(|(name, outcome)| match outcome {
                        Ok(()) => format!("updated {name}"),
                        Err(error) => format!("{name}: {error}"),
                    })
                    .collect();
                let text = if lines.is_empty() {
                    "no marketplaces to update".to_owned()
                } else {
                    lines.join("\n")
                };
                if failed {
                    PluginCommandResult::err(text)
                } else {
                    PluginCommandResult::ok(text)
                }
            }
            Err(err) => PluginCommandResult::err(format!("{err:#}")),
        },
        Some("remove" | "rm") => {
            let Some(name) = args.get(1) else {
                return PluginCommandResult::err(USAGE);
            };
            match marketplaces.remove(name) {
                Ok(()) => PluginCommandResult::ok(format!("removed marketplace {name}")),
                Err(err) => PluginCommandResult::err(format!("{err:#}")),
            }
        }
        Some(_) => PluginCommandResult::err(USAGE),
    }
}

fn plugin_mutate_name(
    installer: &crate::plugin::PluginInstaller,
    args: &[String],
    action: &str,
    f: impl FnOnce(
        &crate::plugin::PluginInstaller,
        &str,
        crate::plugin::PluginScope,
    ) -> anyhow::Result<crate::plugin::InstalledPluginRecord>,
) -> PluginCommandResult {
    let (scope, rest) = parse_plugin_scope_arg(args);
    let scope = match scope {
        Ok(scope) => scope,
        Err(err) => return PluginCommandResult::err(err),
    };
    let Some(name) = rest.first() else {
        return PluginCommandResult::err(format!(
            "Usage: /plugin {action} <name> [--scope user|project]"
        ));
    };
    match f(installer, name, scope) {
        Ok(record) => {
            PluginCommandResult::ok(crate::plugin::format_plugin_result(action, scope, &record))
        }
        Err(err) => PluginCommandResult::err(err.to_string()),
    }
}

fn parse_plugin_scope_arg(
    args: &[String],
) -> (Result<crate::plugin::PluginScope, String>, Vec<String>) {
    let mut scope = crate::plugin::PluginScope::User;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--scope" {
            let Some(raw) = args.get(index + 1) else {
                return (Err("--scope requires user or project".to_string()), rest);
            };
            scope = match crate::plugin::PluginScope::parse(raw) {
                Ok(scope) => scope,
                Err(err) => return (Err(err.to_string()), rest),
            };
            index += 2;
        } else {
            rest.push(args[index].clone());
            index += 1;
        }
    }
    (Ok(scope), rest)
}

/// Pulls `--sha256 <hex>` out of the remaining arguments.
///
/// Kept separate from scope parsing because it is only meaningful for `install`,
/// and a stray `--sha256` on another subcommand should look like the unexpected
/// argument it is rather than being quietly consumed.
fn parse_plugin_sha256_arg(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut digest = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--sha256" {
            let Some(raw) = args.get(index + 1) else {
                return Err("--sha256 requires a hex digest".to_string());
            };
            digest = Some(raw.clone());
            index += 2;
        } else {
            rest.push(args[index].clone());
            index += 1;
        }
    }
    Ok((digest, rest))
}

fn parse_optional_scope(args: &[String]) -> Result<Option<crate::plugin::PluginScope>, String> {
    let (scope, rest) = parse_optional_scope_and_name(args)?;
    if !rest.is_empty() {
        return Err("unexpected arguments after /plugin list".to_string());
    }
    Ok(scope)
}

fn parse_optional_scope_and_name(
    args: &[String],
) -> Result<(Option<crate::plugin::PluginScope>, Vec<String>), String> {
    let mut scope = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--scope" {
            let Some(raw) = args.get(index + 1) else {
                return Err("--scope requires user or project".to_string());
            };
            scope = Some(crate::plugin::PluginScope::parse(raw).map_err(|err| err.to_string())?);
            index += 2;
        } else {
            rest.push(args[index].clone());
            index += 1;
        }
    }
    Ok((scope, rest))
}

fn tokenize_plugin_args(input: &str) -> Result<Vec<String>, String> {
    tokenize_command_args(input, "/ultraplan").map_err(|err| err.replace("/ultraplan", "/plugin"))
}

fn plugin_usage_text() -> String {
    [
        "Usage:",
        "  /plugin install <path|archive|name|rust-lsp> [--scope user|project] [--sha256 <hex>] [--replace-source]",
        "  /plugin list [--scope user|project]",
        "  /plugin status [name] [--scope user|project]",
        "  /plugin verify [name] [--scope user|project]",
        "  /plugin enable <name> [--scope user|project]",
        "  /plugin disable <name> [--scope user|project]",
        "  /plugin uninstall <name> [--scope user|project]",
        "  /plugin browse [marketplace]",
        "  /plugin install <plugin@marketplace>",
        "  /plugin marketplace add <owner/repo|git url|url|path>",
        "  /plugin marketplace list | update [name] | remove <name>",
        "",
        "Capabilities are materialized on next startup.",
    ]
    .join("\n")
}

fn format_plugin_records(
    records: Vec<(
        crate::plugin::PluginScope,
        crate::plugin::InstalledPluginRecord,
    )>,
) -> String {
    if records.is_empty() {
        return "No plugins installed.".to_string();
    }
    records
        .into_iter()
        .map(|(scope, record)| {
            let state = if record.enabled {
                "enabled"
            } else {
                "disabled"
            };
            let source = record
                .source
                .as_deref()
                .unwrap_or(match record.source_kind {
                    crate::plugin::store::PluginSourceKind::Local => "local",
                    crate::plugin::store::PluginSourceKind::Builtin => "builtin",
                });
            format!(
                "{} {}  {}  {}  {}",
                record.name,
                record.version,
                scope.as_str(),
                state,
                source
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_marketplace_adds_updates_and_installs_go_to_the_network() {
        let cwd = std::env::temp_dir();
        assert!(plugin_command_needs_network(
            "/plugin marketplace add o/r",
            &cwd
        ));
        assert!(plugin_command_needs_network(
            "/plugin marketplace update",
            &cwd
        ));
        assert!(plugin_command_needs_network(
            "/plugin install radar@mods",
            &cwd
        ));
        assert!(plugin_command_needs_network(
            "/plugin install --scope user radar@mods",
            &cwd
        ));
        assert!(!plugin_command_needs_network(
            "/plugin install ./local",
            &cwd
        ));
        assert!(!plugin_command_needs_network(
            "/plugin marketplace list",
            &cwd
        ));
        assert!(!plugin_command_needs_network("/plugin browse", &cwd));
        assert!(!plugin_command_needs_network("/plugin list", &cwd));
    }

    #[test]
    fn marketplace_subcommands_run_against_the_context_they_are_given() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let work = dir.path().join("work");
        let market = work.join("market");
        std::fs::create_dir_all(market.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(market.join("radar/.claude-plugin")).unwrap();
        std::fs::create_dir_all(market.join("radar/hooks")).unwrap();
        std::fs::write(
            market.join("radar/.claude-plugin/plugin.json"),
            r#"{"name":"radar"}"#,
        )
        .unwrap();
        std::fs::write(
            market.join("radar/hooks/hooks.json"),
            r#"{"modules":["./r.ts"]}"#,
        )
        .unwrap();
        std::fs::write(
            market.join("radar/hooks/r.ts"),
            "export const register = () => {};",
        )
        .unwrap();
        std::fs::write(
            market.join(".claude-plugin/marketplace.json"),
            r#"{"name":"team","plugins":[{"name":"radar","source":"./radar","description":"live line"}]}"#,
        )
        .unwrap();
        let context = PluginCommandContext {
            cwd: work.clone(),
            plugin_dirs: Vec::new(),
            config_home: home.clone(),
        };
        let run = |text: &str| handle_plugin_command_in(text, &context);
        let added = run("/plugin marketplace add ./market");
        assert!(!added.is_err, "{}", added.text);
        assert!(
            added.text.contains("added marketplace team (1 plugins)"),
            "{}",
            added.text
        );
        let listed = run("/plugin marketplace list");
        assert!(listed.text.contains("team  1 plugins"), "{}", listed.text);
        let browsed = run("/plugin browse team");
        assert!(
            browsed.text.starts_with("radar@team  mod  live line"),
            "{}",
            browsed.text
        );
        let installed = run("/plugin install radar@team");
        assert!(!installed.is_err, "{}", installed.text);
        assert!(home.join("mods/radar/hooks/r.ts").is_file());
        assert!(run("/plugin browse").text.contains("[installed]"));
        let uninstalled = run("/plugin uninstall radar@team");
        assert!(!uninstalled.is_err, "{}", uninstalled.text);
        assert!(!home.join("mods/radar").exists());
        let removed = run("/plugin marketplace remove team");
        assert!(!removed.is_err, "{}", removed.text);
        assert!(run("/plugin marketplace frobnicate").is_err);
        assert!(run("/plugin marketplace add").is_err);
    }

    #[test]
    fn plugin_replace_source_requires_install_and_reaches_local_installer() {
        let _guard = rebon_tool::tasks::test_support::TestConfigHome::new("slash-replace-source");
        let tmp = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            let dir = tmp.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(
                dir.join("rebon-plugin.json"),
                r#"{"name":"demo","version":"1.0.0"}"#,
            )
            .unwrap();
        }
        let context = PluginCommandContext {
            cwd: tmp.path().to_path_buf(),
            plugin_dirs: vec![],
            config_home: tmp.path().join("home"),
        };
        let run = |text| handle_plugin_command_in(text, &context);
        assert!(!run("/plugin install ./a").is_err);
        assert!(run("/plugin install ./b").is_err);
        let result = run("/plugin install --replace-source ./b");
        assert!(!result.is_err, "{}", result.text);
        assert!(!run("/plugin install ./a --replace-source").is_err);
        assert!(run("/plugin list --replace-source").is_err);
        assert!(run("/plugin install --replace-source").is_err);
    }

    #[test]
    fn plugin_sha256_flag_is_lifted_out_of_the_arguments() {
        let args: Vec<String> = ["--sha256", "abc", "demo.tgz"]
            .into_iter()
            .map(String::from)
            .collect();
        let (digest, rest) = parse_plugin_sha256_arg(&args).unwrap();
        assert_eq!(digest.as_deref(), Some("abc"));
        assert_eq!(rest, vec!["demo.tgz".to_string()]);

        // …in either order, since the source is positional.
        let args: Vec<String> = ["demo.tgz", "--sha256", "abc"]
            .into_iter()
            .map(String::from)
            .collect();
        let (digest, rest) = parse_plugin_sha256_arg(&args).unwrap();
        assert_eq!(digest.as_deref(), Some("abc"));
        assert_eq!(rest, vec!["demo.tgz".to_string()]);
    }

    #[test]
    fn plugin_sha256_without_a_value_is_an_error_not_a_silent_none() {
        let args = vec!["demo.tgz".to_string(), "--sha256".to_string()];
        let error = parse_plugin_sha256_arg(&args).unwrap_err();
        assert!(error.contains("requires a hex digest"), "{error}");
    }

    #[test]
    fn plugin_arguments_without_the_flag_pass_through_untouched() {
        let args = vec!["demo.tgz".to_string()];
        let (digest, rest) = parse_plugin_sha256_arg(&args).unwrap();
        assert!(digest.is_none());
        assert_eq!(rest, args);
    }

    #[test]
    fn plugin_usage_lists_the_package_surface() {
        let usage = plugin_usage_text();
        assert!(usage.contains("/plugin verify"), "{usage}");
        assert!(usage.contains("--sha256"), "{usage}");
        assert!(usage.contains("archive"), "{usage}");
    }
}
