use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use rebon_types::{AppVisualEffectManifest, KernelPluginManifest};

use crate::acp_agent_manifest::AcpAgentManifest;
use crate::model_provider_manifest::ModelProviderManifest;
use crate::security::{
    ensure_path_inside_root, reject_dangerous_shell_pipeline, validate_identifier,
    validate_placeholders, validate_relative_asset_path, validate_stdio_command,
};

pub const PLUGIN_MANIFEST_FILE: &str = "rebon-plugin.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub capabilities: PluginCapabilities,
    #[serde(default)]
    pub requirements: PluginRequirements,
    #[serde(default)]
    pub integrity: Option<PluginIntegrity>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginCapabilities {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, Value>,
    #[serde(default, rename = "modelProviders")]
    pub model_providers: BTreeMap<String, ModelProviderManifest>,
    #[serde(default, rename = "acpAgents")]
    pub acp_agents: BTreeMap<String, AcpAgentManifest>,
    #[serde(default, rename = "appVisualEffects")]
    pub app_visual_effects: Vec<AppVisualEffectManifest>,
    /// Plugins this package puts on the kernel plugin plane, keyed by the name
    /// a composition refers to.
    ///
    /// The declaration is a ceiling, not a request: what a plugin registers and
    /// what it may reach are both checked against it at load time, so this is
    /// the field a person reads before installing a package that runs code
    /// inside the plugin host.
    #[serde(default, rename = "kernelPlugins")]
    pub kernel_plugins: BTreeMap<String, KernelPluginManifest>,
    #[serde(default)]
    pub skills: Vec<PathCapability>,
    #[serde(default)]
    pub commands: Vec<PathCapability>,
    #[serde(default)]
    pub workflows: Vec<WorkflowCapability>,
    #[serde(default)]
    pub hooks: Vec<PathCapability>,
    #[serde(default)]
    pub agents: Vec<PathCapability>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PathCapability {
    Path(String),
    Detailed { name: Option<String>, path: String },
}

impl PathCapability {
    pub fn path(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::Detailed { path, .. } => path,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Detailed { name, .. } => name.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum WorkflowCapability {
    Path(String),
    Detailed { name: String, path: String },
}

impl WorkflowCapability {
    pub fn path(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::Detailed { path, .. } => path,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Detailed { name, .. } => Some(name),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginRequirements {
    #[serde(default)]
    pub external_commands: Vec<String>,
    /// Package-relative paths to native extension binaries the package carries.
    ///
    /// A package must declare these, because a native module is the one kind of
    /// payload Rebon cannot reason about: platform- and ABI-specific machine
    /// code with unknown linkage. Installing is fail-closed on both sides of the
    /// declaration.
    #[serde(default)]
    pub native_modules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginIntegrity {
    pub algorithm: String,
    pub digest: String,
}

impl PluginManifest {
    pub fn load_from_dir(root: &Path) -> anyhow::Result<Self> {
        let path = root.join(PLUGIN_MANIFEST_FILE);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read plugin manifest {}", path.display()))?;
        let manifest: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse plugin manifest {}", path.display()))?;
        manifest.validate(Some(root))?;
        Ok(manifest)
    }

    pub fn validate(&self, root: Option<&Path>) -> anyhow::Result<()> {
        validate_identifier("plugin name", &self.name)?;
        validate_identifier("plugin version", &self.version)?;
        if self.name == "rust-lsp" {
            bail!("`rust-lsp` is reserved for Rebon's built-in plugin alias");
        }
        for command in &self.requirements.external_commands {
            validate_identifier("external command requirement", command)?;
        }
        validate_mcp_servers(
            &self.capabilities.mcp_servers,
            &self.requirements.external_commands,
        )?;
        validate_model_providers(
            &self.capabilities.model_providers,
            root,
            &self.requirements.external_commands,
        )?;
        for (id, agent) in &self.capabilities.acp_agents {
            agent.validate(id, root, &self.requirements.external_commands)?;
        }
        for effect in &self.capabilities.app_visual_effects {
            validate_identifier("app visual effect id", &effect.id)?;
            effect.validate().map_err(|message| {
                anyhow!("invalid app visual effect `{}`: {message}", effect.id)
            })?;
        }
        for (name, plugin) in &self.capabilities.kernel_plugins {
            validate_identifier("kernel plugin name", name)?;
            plugin
                .validate_for_package(name)
                .map_err(|message| anyhow!("invalid kernel plugin `{name}`: {message}"))?;
            // The entry is a module inside this package, and the same rule
            // every other packaged asset follows: relative, and not out of the
            // package.
            let entry = plugin.entry.as_deref().unwrap_or_default();
            let relative = validate_relative_asset_path(entry).map_err(|error| {
                anyhow!("invalid kernel plugin `{name}` entry `{entry}`: {error}")
            })?;
            if let Some(root) = root {
                ensure_path_inside_root(root, &relative).map_err(|error| {
                    anyhow!("invalid kernel plugin `{name}` entry `{entry}`: {error}")
                })?;
            }
        }
        validate_path_capabilities("skill", &self.capabilities.skills, root)?;
        validate_path_capabilities("command", &self.capabilities.commands, root)?;
        validate_path_capabilities("hook", &self.capabilities.hooks, root)?;
        validate_hook_capabilities(&self.capabilities.hooks, root)?;
        validate_path_capabilities("agent", &self.capabilities.agents, root)?;
        validate_workflow_capabilities(&self.capabilities.workflows, root)?;
        validate_duplicate_names(
            "skill",
            self.capabilities.skills.iter().map(path_capability_id),
        )?;
        validate_duplicate_names(
            "command",
            self.capabilities.commands.iter().map(path_capability_id),
        )?;
        validate_duplicate_names(
            "workflow",
            self.capabilities
                .workflows
                .iter()
                .map(workflow_capability_id),
        )?;
        validate_duplicate_names(
            "agent",
            self.capabilities.agents.iter().map(path_capability_id),
        )?;
        validate_duplicate_names("MCP server", self.capabilities.mcp_servers.keys().cloned())?;
        validate_duplicate_names(
            "model provider",
            self.capabilities.model_providers.keys().cloned(),
        )?;
        validate_duplicate_names("ACP agent", self.capabilities.acp_agents.keys().cloned())?;
        validate_duplicate_names(
            "app visual effect",
            self.capabilities
                .app_visual_effects
                .iter()
                .map(|effect| effect.id.clone()),
        )?;
        validate_duplicate_names(
            "kernel plugin",
            self.capabilities.kernel_plugins.keys().cloned(),
        )?;
        Ok(())
    }

    /// The capability names this package occupies.
    ///
    /// Used to decide which of two packages offering the same thing wins, so
    /// the granularity matters: shadowing per package would drop a loser's
    /// unrelated capabilities along with the contested one.
    ///
    /// `appVisualEffects` is deliberately absent — the app resolves those by
    /// surface and priority rather than by install precedence.
    pub fn capability_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        ids.extend(
            self.capabilities
                .mcp_servers
                .keys()
                .map(|name| format!("mcp:{name}")),
        );
        ids.extend(
            self.capabilities
                .model_providers
                .keys()
                .map(|name| format!("model-provider:{name}")),
        );
        ids.extend(
            self.capabilities
                .acp_agents
                .keys()
                .map(|name| format!("acp-agent:{name}")),
        );
        ids.extend(
            self.capabilities
                .kernel_plugins
                .keys()
                .map(|name| format!("kernel-plugin:{name}")),
        );
        for (kind, paths) in [
            ("skill", &self.capabilities.skills),
            ("command", &self.capabilities.commands),
            ("hook", &self.capabilities.hooks),
            ("agent", &self.capabilities.agents),
        ] {
            ids.extend(
                paths
                    .iter()
                    .map(|cap| format!("{kind}:{}", cap.name().unwrap_or(cap.path()))),
            );
        }
        ids.extend(
            self.capabilities
                .workflows
                .iter()
                .map(|cap| format!("workflow:{}", cap.name().unwrap_or(cap.path()))),
        );
        ids
    }

    #[cfg(test)]
    pub fn capability_summary(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.capabilities.mcp_servers.is_empty() {
            out.push(format!(
                "{} MCP server(s)",
                self.capabilities.mcp_servers.len()
            ));
        }
        if !self.capabilities.model_providers.is_empty() {
            out.push(format!(
                "{} model provider(s)",
                self.capabilities.model_providers.len()
            ));
        }
        if !self.capabilities.acp_agents.is_empty() {
            out.push(format!(
                "{} ACP agent(s)",
                self.capabilities.acp_agents.len()
            ));
        }
        if !self.capabilities.app_visual_effects.is_empty() {
            out.push(format!(
                "{} app visual effect(s)",
                self.capabilities.app_visual_effects.len()
            ));
        }
        if !self.capabilities.kernel_plugins.is_empty() {
            out.push(format!(
                "{} kernel plugin(s)",
                self.capabilities.kernel_plugins.len()
            ));
        }
        if !self.capabilities.skills.is_empty() {
            out.push(format!("{} skill path(s)", self.capabilities.skills.len()));
        }
        if !self.capabilities.commands.is_empty() {
            out.push(format!(
                "{} command path(s)",
                self.capabilities.commands.len()
            ));
        }
        if !self.capabilities.workflows.is_empty() {
            out.push(format!(
                "{} workflow path(s)",
                self.capabilities.workflows.len()
            ));
        }
        if !self.capabilities.hooks.is_empty() {
            out.push(format!("{} hook path(s)", self.capabilities.hooks.len()));
        }
        if !self.capabilities.agents.is_empty() {
            out.push(format!("{} agent path(s)", self.capabilities.agents.len()));
        }
        if out.is_empty() {
            out.push("no runtime capabilities".to_string());
        }
        out
    }
}

fn validate_path_capabilities<'a>(
    kind: &str,
    capabilities: impl IntoIterator<Item = &'a PathCapability>,
    root: Option<&Path>,
) -> anyhow::Result<()> {
    for capability in capabilities {
        if let Some(name) = capability.name() {
            validate_identifier(kind, name)?;
        }
        let relative = validate_relative_asset_path(capability.path())?;
        if let Some(root) = root {
            ensure_path_inside_root(root, &root.join(relative))?;
        }
    }
    Ok(())
}

fn validate_workflow_capabilities(
    capabilities: &[WorkflowCapability],
    root: Option<&Path>,
) -> anyhow::Result<()> {
    for capability in capabilities {
        if let Some(name) = capability.name() {
            validate_identifier("workflow", name)?;
        }
        let relative = validate_relative_asset_path(capability.path())?;
        if let Some(root) = root {
            ensure_path_inside_root(root, &root.join(relative))?;
        }
    }
    Ok(())
}

fn validate_hook_capabilities(
    capabilities: &[PathCapability],
    root: Option<&Path>,
) -> anyhow::Result<()> {
    let Some(root) = root else {
        return Ok(());
    };
    for capability in capabilities {
        let path = root.join(capability.path());
        let loaded = rebon_hooks::load_hooks_from_path(&path, rebon_hooks::HookSource::PluginHook)
            .with_context(|| format!("failed to validate plugin hook file {}", path.display()))?;
        if let Some(warning) = loaded.warnings.first() {
            anyhow::bail!(
                "invalid plugin hook in {}: {}",
                path.display(),
                warning.reason
            );
        }
        for hook in loaded.hooks {
            if let rebon_hooks::HookCommand::Command(command) = hook.config {
                reject_dangerous_shell_pipeline(&command.command)?;
            }
        }
    }
    Ok(())
}

fn validate_mcp_servers(
    servers: &BTreeMap<String, Value>,
    external_commands: &[String],
) -> anyhow::Result<()> {
    for (name, value) in servers {
        validate_identifier("MCP server name", name)?;
        let Some(obj) = value.as_object() else {
            bail!("MCP server `{name}` must be an object");
        };
        if obj.contains_key("headersHelper") {
            bail!("unsupported MCP server `{name}`: `headersHelper` is not supported");
        }
        if obj.contains_key("oauth") {
            bail!("unsupported MCP server `{name}`: `oauth` is not supported");
        }
        if let Some(command) = obj.get("command").and_then(Value::as_str) {
            validate_stdio_command(command, external_commands)?;
        }
        if let Some(args) = obj.get("args") {
            let Some(args) = args.as_array() else {
                bail!("MCP server `{name}` `args` must be an array of strings");
            };
            for arg in args {
                let Some(arg) = arg.as_str() else {
                    bail!("MCP server `{name}` `args` must be an array of strings");
                };
                validate_placeholders(arg)?;
            }
        }
        if let Some(cwd) = obj.get("cwd").and_then(Value::as_str) {
            validate_placeholders(cwd)?;
        }
    }
    Ok(())
}

fn validate_model_providers(
    providers: &BTreeMap<String, ModelProviderManifest>,
    root: Option<&Path>,
    external_commands: &[String],
) -> anyhow::Result<()> {
    for (id, provider) in providers {
        provider.validate(id, root, external_commands)?;
    }
    Ok(())
}

fn path_capability_id(capability: &PathCapability) -> String {
    capability
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| path_stem_id(capability.path()))
}

fn workflow_capability_id(capability: &WorkflowCapability) -> String {
    capability
        .name()
        .map(str::to_string)
        .unwrap_or_else(|| path_stem_id(capability.path()))
}

fn path_stem_id(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn validate_duplicate_names(
    kind: &str,
    names: impl IntoIterator<Item = String>,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            return Err(anyhow!("duplicate {kind} capability `{name}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(value: serde_json::Value) -> PluginManifest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn validates_first_class_capabilities() {
        let manifest = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {
                "mcpServers": {
                    "demo_mcp": {"command": "{rebon}", "args": ["lsp-mcp", "rust"]}
                },
                "skills": ["skills/demo"],
                "commands": [{"name": "do-demo", "path": "commands/do-demo.md"}],
                "workflows": [{"name": "flow", "path": "workflows/flow.md"}],
                "hooks": ["hooks/hooks.json"],
                "agents": ["agents/demo.md"],
                "appVisualEffects": [{
                    "id": "black-hole",
                    "surface": "chatEmpty",
                    "kind": "blackHole"
                }],
                "metadata": {"reserved": true}
            }
        }));

        manifest.validate(None).unwrap();
        assert_eq!(manifest.capability_summary().len(), 7);
    }

    /// The whole reason `kernelPlugins` is a capability: one package, one
    /// manifest, and the plane reads the same file the installer validated.
    #[test]
    fn accepts_a_kernel_plugin_beside_the_other_capabilities() {
        let manifest = manifest_json(serde_json::json!({
            "name": "plane-demo",
            "version": "1.0.0",
            "capabilities": {
                "mcpServers": {"demo": {"command": "{plugin_dir}/bin/demo"}},
                "kernelPlugins": {
                    "demo-plane": {
                        "entry": "plane/index.mjs",
                        "services": ["echo"],
                        "eventTopics": ["session"],
                        "publishedTopics": ["demo:tick"],
                        "llmProviders": ["demo"],
                        "tools": ["demo_tool"],
                        "invokableTools": "$rebon/tools",
                        "seats": ["logger"]
                    }
                }
            }
        }));

        manifest.validate(None).unwrap();
        let plugin = &manifest.capabilities.kernel_plugins["demo-plane"];
        assert_eq!(plugin.entry.as_deref(), Some("plane/index.mjs"));
        assert_eq!(plugin.seats, vec!["logger".to_string()]);
        assert!(manifest
            .capability_summary()
            .iter()
            .any(|line| line.contains("kernel plugin")));
    }

    #[test]
    fn rejects_a_kernel_plugin_with_no_entry_module() {
        let manifest = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {"kernelPlugins": {"broken": {"services": ["echo"]}}}
        }));
        let error = manifest.validate(None).unwrap_err().to_string();
        assert!(error.contains("no entry module"), "{error}");
    }

    /// An entry is a module inside the package, held to the same rule as every
    /// other packaged asset.
    #[test]
    fn rejects_a_kernel_plugin_entry_that_escapes_the_package() {
        let manifest = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {"kernelPlugins": {"sneaky": {"entry": "../elsewhere/index.mjs"}}}
        }));
        assert!(manifest.validate(None).is_err());
    }

    /// `payload` and `runtime` name rebon's own packages. A third-party package
    /// pointing there would be loading rebon's files under its own declared
    /// ceiling.
    #[test]
    fn rejects_a_kernel_plugin_rooted_in_rebons_own_packages() {
        let manifest = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {
                "kernelPlugins": {"sneaky": {"root": "payload", "entry": "index.mjs"}}
            }
        }));
        let error = manifest.validate(None).unwrap_err().to_string();
        assert!(error.contains("rebon's own packages"), "{error}");
    }

    #[test]
    fn rejects_unsupported_mcp_fields() {
        let manifest = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {
                "mcpServers": {
                    "bad": {"command": "{rebon}", "headersHelper": "helper"}
                }
            }
        }));

        assert!(manifest.validate(None).is_err());
    }

    #[test]
    fn rejects_unknown_placeholders_and_traversal() {
        let placeholder = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {"mcpServers": {"bad": {"command": "{home}/x"}}}
        }));
        assert!(placeholder.validate(None).is_err());

        let traversal = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {"skills": ["../outside"]}
        }));
        assert!(traversal.validate(None).is_err());
    }

    #[test]
    fn validates_visual_effect_defaults_and_summary() {
        let manifest = manifest_json(serde_json::json!({
            "name": "visual-demo",
            "version": "1.0.0",
            "capabilities": {
                "appVisualEffects": [{
                    "id": "black-hole",
                    "surface": "chatEmpty",
                    "kind": "blackHole"
                }]
            }
        }));

        manifest.validate(None).unwrap();
        assert_eq!(
            manifest.capability_summary(),
            vec!["1 app visual effect(s)".to_string()]
        );
        assert!(
            manifest.capabilities.app_visual_effects[0]
                .options
                .auto_rotate
        );
    }

    #[test]
    fn rejects_duplicate_visual_effect_ids() {
        let manifest = manifest_json(serde_json::json!({
            "name": "visual-demo",
            "version": "1.0.0",
            "capabilities": {
                "appVisualEffects": [
                    {"id": "same", "surface": "chatEmpty", "kind": "blackHole"},
                    {"id": "same", "surface": "chatEmpty", "kind": "blackHole"}
                ]
            }
        }));

        assert!(manifest.validate(None).is_err());
    }

    #[test]
    fn rejects_invalid_visual_effect_options() {
        let manifest = manifest_json(serde_json::json!({
            "name": "visual-demo",
            "version": "1.0.0",
            "capabilities": {
                "appVisualEffects": [{
                    "id": "black-hole",
                    "surface": "chatEmpty",
                    "kind": "blackHole",
                    "options": {"rotationSeconds": 2.0}
                }]
            }
        }));

        assert!(manifest.validate(None).is_err());
    }

    #[test]
    fn rejects_unknown_visual_effect_surface_and_kind() {
        for effect in [
            serde_json::json!({"id": "bad", "surface": "dashboard", "kind": "blackHole"}),
            serde_json::json!({"id": "bad", "surface": "chatEmpty", "kind": "shader"}),
        ] {
            let value = serde_json::json!({
                "name": "visual-demo",
                "version": "1.0.0",
                "capabilities": {"appVisualEffects": [effect]}
            });
            assert!(serde_json::from_value::<PluginManifest>(value).is_err());
        }
    }

    #[test]
    fn bundled_black_hole_visual_plugin_validates() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../runtimes/node/plugins/rebon-black-hole");
        let manifest = PluginManifest::load_from_dir(&root).unwrap();

        assert_eq!(manifest.name, "rebon-black-hole");
        assert_eq!(manifest.capabilities.app_visual_effects.len(), 1);
        assert_eq!(manifest.capabilities.app_visual_effects[0].id, "black-hole");
    }

    #[test]
    fn rejects_duplicate_command_names() {
        let manifest = manifest_json(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "capabilities": {
                "commands": [
                    {"name": "same", "path": "commands/a.md"},
                    {"name": "same", "path": "commands/b.md"}
                ]
            }
        }));
        assert!(manifest.validate(None).is_err());
    }
}
