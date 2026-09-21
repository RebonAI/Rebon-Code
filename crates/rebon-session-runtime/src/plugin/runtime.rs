use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rebon_hooks::{HookSource, IndividualHookConfig};
use serde_json::Value;

use rebon_harness::rebon_plugin_package::discovery::{discover, InstalledPlugin};

use super::acp_agent_manifest::{materialize_acp_agent_contribution, PluginAcpAgentContribution};
use super::builtin::rust_lsp_mcp_config_payload;
use super::manifest::{PathCapability, PluginManifest};
use super::model_provider_manifest::{
    materialize_model_provider_contribution, PluginModelProviderContribution,
};
use super::security::expand_placeholders;
use super::store::PluginStore;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginRuntimeContributions {
    pub mcp_configs: Vec<crate::mcp_config::PluginMcpConfig>,
    pub model_providers: Vec<PluginModelProviderContribution>,
    pub acp_agents: Vec<PluginAcpAgentContribution>,
    pub skill_dirs: Vec<PathBuf>,
    pub command_dirs: Vec<PathBuf>,
    /// `(plugin name, agents dir)` — the name rides along because the
    /// registry cannot recover it from the path (see
    /// `AgentRegistry::load_with_plugin_dirs`).
    pub agent_dirs: Vec<(String, PathBuf)>,
    pub hooks: Vec<IndividualHookConfig>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PluginRuntimeOptions {
    pub cwd: PathBuf,
    pub config_home: PathBuf,
    pub plugin_dirs: Vec<PathBuf>,
    pub rebon_exe: Option<PathBuf>,
}

/// What this session's plugins contribute, materialised and ready to wire in.
///
/// Which plugins are in effect — session directories, a trusted project's
/// installs, the user's installs, and the shadowing between them — is
/// `rebon_plugin_package::discovery`'s answer, because the plugin plane needs
/// the same one. What is left here is turning each of them into something that
/// runs: placeholders expanded, commands built, paths resolved.
pub fn resolve_runtime_contributions(
    options: &PluginRuntimeOptions,
) -> anyhow::Result<PluginRuntimeContributions> {
    let store = PluginStore::new(options.config_home.clone(), options.cwd.clone());
    let project_trusted =
        crate::rebon_config::is_directory_trusted_in(&options.config_home, &options.cwd);
    let found = discover(&store, &options.plugin_dirs, &options.cwd, project_trusted)?;

    let mut out = PluginRuntimeContributions {
        warnings: found.warnings,
        ..PluginRuntimeContributions::default()
    };
    for plugin in &found.plugins {
        match &plugin.plugin {
            InstalledPlugin::Builtin { name } if name == "rust-lsp" => {
                match rust_lsp_mcp_config_payload(&options.cwd, options.rebon_exe.as_deref()) {
                    Ok(payload) => out.mcp_configs.push(crate::mcp_config::PluginMcpConfig {
                        payload: payload.to_string(),
                        source: plugin.source.clone(),
                    }),
                    // A warning, not a hard failure: the alias not finding its
                    // bridge binary must not stop the other plugins in this
                    // session from being wired in.
                    Err(err) => out
                        .warnings
                        .push(format!("built-in plugin alias `rust-lsp` skipped: {err}")),
                }
            }
            InstalledPlugin::Builtin { name } => out
                .warnings
                .push(format!("unknown built-in plugin alias `{name}`")),
            InstalledPlugin::Package { root, manifest } => materialize_manifest(
                &mut out,
                manifest,
                root,
                &plugin.source,
                &options.cwd,
                options.rebon_exe.as_deref(),
            )?,
        }
    }

    Ok(out)
}

fn materialize_manifest(
    out: &mut PluginRuntimeContributions,
    manifest: &PluginManifest,
    root: &Path,
    source: &str,
    _cwd: &Path,
    rebon_exe: Option<&Path>,
) -> anyhow::Result<()> {
    let rebon = rebon_exe
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("rebon"));

    if !manifest.capabilities.mcp_servers.is_empty() {
        let mut servers = BTreeMap::new();
        for (name, raw) in &manifest.capabilities.mcp_servers {
            servers.insert(name.clone(), expand_mcp_value(raw, root, &rebon)?);
        }
        out.mcp_configs.push(crate::mcp_config::PluginMcpConfig {
            payload: serde_json::json!({"mcpServers": servers}).to_string(),
            source: source.to_string(),
        });
    }

    for (provider_id, provider) in &manifest.capabilities.model_providers {
        match materialize_model_provider_contribution(
            &manifest.name,
            provider_id,
            provider,
            root,
            source,
            &rebon,
        ) {
            Ok(contribution) => out.model_providers.push(contribution),
            Err(err) => out.warnings.push(format!(
                "plugin model provider `{provider_id}` from {} skipped: {err}",
                manifest.name
            )),
        }
    }

    for (agent_id, agent) in &manifest.capabilities.acp_agents {
        match materialize_acp_agent_contribution(
            &manifest.name,
            agent_id,
            agent,
            root,
            source,
            &rebon,
        ) {
            Ok(contribution) => out.acp_agents.push(contribution),
            Err(err) => out.warnings.push(format!(
                "plugin ACP agent `{agent_id}` from {} skipped: {err}",
                manifest.name
            )),
        }
    }

    out.skill_dirs.extend(
        manifest
            .capabilities
            .skills
            .iter()
            .map(|capability| root.join(capability.path())),
    );
    out.command_dirs.extend(
        manifest
            .capabilities
            .commands
            .iter()
            .map(|capability| root.join(capability.path())),
    );
    out.command_dirs.extend(
        manifest
            .capabilities
            .workflows
            .iter()
            .map(|capability| root.join(capability.path())),
    );
    out.agent_dirs.extend(
        agent_dirs(root, &manifest.capabilities.agents)
            .into_iter()
            .map(|dir| (manifest.name.clone(), dir)),
    );

    for hook in &manifest.capabilities.hooks {
        let path = root.join(hook.path());
        let loaded = rebon_hooks::load_hooks_from_path(&path, HookSource::PluginHook)
            .with_context(|| format!("failed to load plugin hooks {}", path.display()))?;
        for mut config in loaded.hooks {
            config.plugin_name = Some(manifest.name.clone());
            out.hooks.push(config);
        }
        for warning in loaded.warnings {
            out.warnings
                .push(format!("plugin hook warning: {warning:?}"));
        }
    }

    Ok(())
}

/// Expands a manifest's MCP-server block against an installed plugin.
///
/// Fallible because `{rebon_bin:…}` resolves to a real file: a manifest that
/// names a binary this install does not have fails here, rather than producing
/// an entry that only fails when the runtime tries to spawn it.
fn expand_mcp_value(value: &Value, plugin_dir: &Path, rebon: &Path) -> anyhow::Result<Value> {
    Ok(match value {
        Value::String(s) => Value::String(expand_placeholders(s, plugin_dir, rebon)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| expand_mcp_value(item, plugin_dir, rebon))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| Ok((key.clone(), expand_mcp_value(value, plugin_dir, rebon)?)))
                .collect::<anyhow::Result<serde_json::Map<_, _>>>()?,
        ),
        _ => value.clone(),
    })
}

fn agent_dirs(root: &Path, agents: &[PathCapability]) -> Vec<PathBuf> {
    agents
        .iter()
        .map(|capability| root.join(capability.path()))
        .map(|path| {
            if path.is_file() {
                path.parent().unwrap_or(&path).to_path_buf()
            } else {
                path
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::store::{InstalledPluginRecord, PluginScope, PluginSourceKind, PluginStore};

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set_config_dir(path: &Path) -> Self {
            let _lock = crate::test_env::lock_env();
            let previous = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("REBON_CONFIG_DIR", path);
            Self { previous, _lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.take() {
                std::env::set_var("REBON_CONFIG_DIR", previous);
            } else {
                std::env::remove_var("REBON_CONFIG_DIR");
            }
        }
    }

    #[test]
    fn plugin_dir_materializes_model_provider_contribution() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("provider.mjs"),
            "export function activate() {}",
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("rebon-plugin.json"),
            serde_json::json!({
                "name": "fake-provider-plugin",
                "version": "1.0.0",
                "capabilities": {
                    "modelProviders": {
                        "fake-provider": {
                            "transport": {"type":"plugin", "entry":"provider.mjs"},
                            "defaultModel": "fake-model",
                            "models": {"fake-model": {"contextWindow": 12345}},
                            "profiles": {"small": "fake-small"},
                            "capabilities": {"forcedToolChoice": true}
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let contributions = resolve_runtime_contributions(&PluginRuntimeOptions {
            cwd: tmp.path().join("project"),
            config_home: tmp.path().join("home"),
            plugin_dirs: vec![plugin_dir],
            rebon_exe: Some(PathBuf::from("/bin/rebon")),
        })
        .unwrap();

        assert_eq!(contributions.model_providers.len(), 1);
        let provider = &contributions.model_providers[0];
        assert_eq!(provider.id, "fake-provider");
        assert_eq!(provider.default_model.as_deref(), Some("fake-model"));
        assert!(provider.capabilities.forced_tool_choice);
    }

    /// The in-repo `runtimes/node/plugins/deepseek-responses` package must always pass the
    /// real manifest validation chain (identifier rules, and the plane
    /// transport's entry actually being a module inside the package).
    #[test]
    fn in_repo_deepseek_responses_plugin_materializes() {
        let repo_plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../runtimes/node/plugins/deepseek-responses");
        assert!(
            repo_plugin.join("rebon-plugin.json").is_file(),
            "plugin manifest missing at {repo_plugin:?}"
        );
        let tmp = tempfile::tempdir().unwrap();
        let contributions = resolve_runtime_contributions(&PluginRuntimeOptions {
            cwd: tmp.path().join("project"),
            config_home: tmp.path().join("home"),
            plugin_dirs: vec![repo_plugin],
            rebon_exe: Some(PathBuf::from("/bin/rebon")),
        })
        .unwrap();

        assert_eq!(contributions.model_providers.len(), 1);
        let provider = &contributions.model_providers[0];
        assert_eq!(provider.id, "deepseek");
        assert_eq!(provider.default_model.as_deref(), Some("deepseek-v4-pro"));
        // The `small` profile drives compaction summaries; it stays on Flash so
        // a Pro default does not make every compaction cost Pro output rates.
        assert_eq!(provider.profiles.get("small"), Some("deepseek-v4-flash"));
        assert_eq!(
            provider
                .models
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["deepseek-v4-flash", "deepseek-v4-pro"]
        );
        assert_eq!(
            provider.models["deepseek-v4-pro"].context_window,
            Some(1_000_000)
        );
        assert!(provider.capabilities.forced_tool_choice);
        assert!(provider.capabilities.reasoning_text);
        assert!(provider.capabilities.custom_tool_call);
        assert!(provider.capabilities.web_search);
        assert!(provider.capabilities.anchored_minimal);
        assert!(!provider.capabilities.stateful_responses);
        assert!(!provider.capabilities.remote_compaction);
        let crate::plugin::model_provider_manifest::MaterializedModelProviderTransport::Plugin(
            plugin,
        ) = &provider.transport;
        assert_eq!(plugin.entry, "provider.mjs");
    }

    #[test]
    fn plugin_dir_materializes_acp_agent_contribution() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("rebon-plugin.json"),
            serde_json::json!({
                "name": "claudecode-plugin",
                "version": "1.0.0",
                "requirements": {"externalCommands": ["claude-agent-acp"]},
                "capabilities": {
                    "acpAgents": {
                        "claudecode": {
                            "command": "claude-agent-acp",
                            "displayName": "Claude Code",
                            "sessionMeta": {"claudeCode": {"options": {"disallowedTools": ["Write"]}}},
                            "installHint": "npm install -g @agentclientprotocol/claude-agent-acp"
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let contributions = resolve_runtime_contributions(&PluginRuntimeOptions {
            cwd: tmp.path().join("project"),
            config_home: tmp.path().join("home"),
            plugin_dirs: vec![plugin_dir],
            rebon_exe: Some(PathBuf::from("/bin/rebon")),
        })
        .unwrap();

        assert_eq!(contributions.acp_agents.len(), 1);
        let agent = &contributions.acp_agents[0];
        assert_eq!(agent.id, "claudecode");
        assert_eq!(agent.plugin_name, "claudecode-plugin");
        assert_eq!(agent.display_name.as_deref(), Some("Claude Code"));
        assert!(agent.inject_fs_tools);
        assert!(agent
            .session_meta
            .as_ref()
            .is_some_and(|meta| meta.contains_key("claudeCode")));
        assert!(agent
            .install_hint
            .as_deref()
            .is_some_and(|hint| hint.contains("npm install")));
    }

    #[test]
    fn builtin_rust_lsp_record_materializes_mcp_contribution() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        store
            .upsert_record(
                PluginScope::User,
                InstalledPluginRecord {
                    name: "rust-lsp".into(),
                    version: "1".into(),
                    enabled: true,
                    disabled_capabilities: Vec::new(),
                    source_kind: PluginSourceKind::Builtin,
                    source: Some("rust-lsp".into()),
                    digest: None,
                    manifest: None,
                },
            )
            .unwrap();

        // The alias materializes a path to the bridge binary beside `rebon`,
        // so the fake install has to actually contain one.
        let install = tmp.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        let bridge = install.join(rebon_types::sibling_binary::file_name("rebon-lsp-mcp"));
        std::fs::write(&bridge, b"binary").unwrap();

        let contributions = resolve_runtime_contributions(&PluginRuntimeOptions {
            cwd: tmp.path().join("project"),
            config_home: tmp.path().join("home"),
            plugin_dirs: Vec::new(),
            rebon_exe: Some(install.join(rebon_types::sibling_binary::file_name("rebon"))),
        })
        .unwrap();
        assert_eq!(contributions.mcp_configs.len(), 1);
        let payload = &contributions.mcp_configs[0].payload;
        assert!(payload.contains("rust_lsp"), "{payload}");
        assert!(
            payload.contains(&bridge.to_string_lossy().replace('\\', "\\\\")),
            "{payload}"
        );
    }

    #[test]
    fn a_missing_bridge_binary_warns_instead_of_failing_the_session() {
        // The alias is one plugin among several. Not finding its binary is a
        // reason to skip it, not a reason for the rest not to load.
        let tmp = tempfile::tempdir().unwrap();
        let store = PluginStore::new(tmp.path().join("home"), tmp.path().join("project"));
        store
            .upsert_record(
                PluginScope::User,
                InstalledPluginRecord {
                    name: "rust-lsp".into(),
                    version: "1".into(),
                    enabled: true,
                    disabled_capabilities: Vec::new(),
                    source_kind: PluginSourceKind::Builtin,
                    source: Some("rust-lsp".into()),
                    digest: None,
                    manifest: None,
                },
            )
            .unwrap();

        let contributions = resolve_runtime_contributions(&PluginRuntimeOptions {
            cwd: tmp.path().join("project"),
            config_home: tmp.path().join("home"),
            plugin_dirs: Vec::new(),
            rebon_exe: Some(tmp.path().join("empty").join("rebon")),
        })
        .unwrap();
        assert!(contributions.mcp_configs.is_empty());
        assert!(
            contributions
                .warnings
                .iter()
                .any(|warning| warning.contains("rust-lsp")),
            "{:?}",
            contributions.warnings
        );
    }

    #[test]
    fn project_disabled_suppresses_user_builtin_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let config_home = tmp.path().join("home");
        std::fs::create_dir_all(&config_home).unwrap();
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let key = cwd.to_string_lossy().replace('\\', "/");
        let key = if cfg!(windows) {
            key.to_ascii_lowercase()
        } else {
            key
        };
        std::fs::write(
            config_home.join("config.json"),
            serde_json::json!({"projects": {key: {"hasTrustDialogAccepted": true}}}).to_string(),
        )
        .unwrap();
        let _guard = EnvGuard::set_config_dir(&config_home);
        let store = PluginStore::new(config_home.clone(), cwd.clone());
        let user = InstalledPluginRecord {
            name: "rust-lsp".into(),
            version: "1".into(),
            enabled: true,
            disabled_capabilities: Vec::new(),
            source_kind: PluginSourceKind::Builtin,
            source: Some("rust-lsp".into()),
            digest: None,
            manifest: None,
        };
        let mut project = user.clone();
        project.enabled = false;
        store.upsert_record(PluginScope::User, user).unwrap();
        store.upsert_record(PluginScope::Project, project).unwrap();

        let contributions = resolve_runtime_contributions(&PluginRuntimeOptions {
            cwd,
            config_home,
            plugin_dirs: Vec::new(),
            rebon_exe: Some(PathBuf::from("/bin/rebon")),
        })
        .unwrap();
        assert!(contributions.mcp_configs.is_empty());
    }
}
