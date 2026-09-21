//! `/plugin` -- what is installed, and installing or removing one.
//!
//! Listing, installing from a path or a name, verifying a digest, enabling,
//! disabling and removing all run against the plugin store on disk. The one
//! thing the command reads from the session is its working directory, which
//! decides what "project scope" means, so this belongs beside the session
//! rather than inside the view that happens to type it.

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

pub fn handle_plugin_command(text: &str, session: &EngineSession) -> PluginCommandResult {
    let args = command_args(text, "plugin");
    let tokens = match tokenize_plugin_args(args) {
        Ok(tokens) => tokens,
        Err(err) => return PluginCommandResult::err(err),
    };
    if tokens.is_empty() {
        return PluginCommandResult::ok(plugin_usage_text());
    }

    let cwd = PathBuf::from(&session.cwd);
    let store =
        crate::plugin::PluginStore::new(crate::rebon_config::config_home_dir(), cwd.clone());
    let installer = crate::plugin::PluginInstaller::new(
        store,
        cwd,
        session
            .startup
            .plugin_dirs
            .iter()
            .map(PathBuf::from)
            .collect(),
    );
    match tokens[0].as_str() {
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
                    "Usage: /plugin install <path|archive|name|rust-lsp> [--scope user|project] [--sha256 <hex>]",
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
        "  /plugin install <path|archive|name|rust-lsp> [--scope user|project] [--sha256 <hex>]",
        "  /plugin list [--scope user|project]",
        "  /plugin status [name] [--scope user|project]",
        "  /plugin verify [name] [--scope user|project]",
        "  /plugin enable <name> [--scope user|project]",
        "  /plugin disable <name> [--scope user|project]",
        "  /plugin uninstall <name> [--scope user|project]",
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
