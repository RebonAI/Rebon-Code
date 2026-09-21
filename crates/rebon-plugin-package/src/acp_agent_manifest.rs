//! `capabilities.acpAgents` — plugin-declared ACP agent CLIs.
//!
//! A plugin describes how to *start* an agent CLI (command/args/env,
//! adapter options via `sessionMeta`); it never installs the CLI
//! itself. The shape mirrors the app config's ACP agent entry field
//! for field so the config.json surface and the plugin surface stay
//! one vocabulary, and the validation + placeholder story is the shared
//! one in [`crate::security`] — `validate_stdio_command` plus the
//! placeholder helpers, the same primitives the MCP servers use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::{
    ensure_path_inside_root, expand_placeholders, validate_identifier, validate_placeholders,
    validate_stdio_command,
};

/// One `capabilities.acpAgents` entry, keyed by agent id in the map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentManifest {
    /// Executable to run. `{plugin_dir}`/`{rebon}` placeholders work,
    /// so a plugin can ship its own agent binary; a bare command name
    /// must be listed in `requirements.externalCommands`.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child. `$VAR` indirection resolves at
    /// spawn time like config.json entries. (`${VAR}` is not available
    /// here — the brace form collides with the plugin placeholder
    /// syntax and is rejected by validation.)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory. Relative paths resolve inside the plugin
    /// root; omitted means "the session's".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether rebon's filesystem tools are injected into the child;
    /// omitted means yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_fs_tools: Option<bool>,
    /// `_meta` for `session/new`/`session/load` — adapter options like
    /// `claudeCode.options.disallowedTools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_meta: Option<BTreeMap<String, Value>>,
    /// Shown when the command cannot be spawned. Plugins declare
    /// agents, they do not install them — this is where "npm install
    /// -g @agentclientprotocol/claude-agent-acp" lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
}

impl AcpAgentManifest {
    pub fn validate(
        &self,
        agent_id: &str,
        root: Option<&Path>,
        external_commands: &[String],
    ) -> anyhow::Result<()> {
        validate_identifier("ACP agent id", agent_id)?;
        if agent_id.eq_ignore_ascii_case("local") {
            bail!("ACP agent id `{agent_id}` is reserved for Rebon's own engine");
        }
        validate_stdio_command(&self.command, external_commands)
            .with_context(|| format!("invalid ACP agent `{agent_id}` command"))?;
        for arg in &self.args {
            validate_placeholders(arg)?;
        }
        for (key, value) in &self.env {
            validate_env_key(key)?;
            validate_placeholders(value)?;
        }
        if let Some(cwd) = self.cwd.as_deref() {
            validate_placeholders(cwd)?;
            if let Some(root) = root {
                let expanded = expand_placeholders(cwd, root, Path::new("rebon"))?;
                let path = PathBuf::from(expanded);
                let candidate = if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                };
                ensure_path_inside_root(root, &candidate)?;
            }
        }
        Ok(())
    }
}

/// A materialized ACP agent declaration: placeholders expanded against
/// the installed plugin's root, ready to be folded into the session's
/// declared-agent list next to config.json and frontmatter entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAcpAgentContribution {
    pub id: String,
    pub plugin_name: String,
    /// Provenance label, e.g. `plugin:name@user`.
    pub source: String,
    pub display_name: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub inject_fs_tools: bool,
    pub session_meta: Option<BTreeMap<String, Value>>,
    pub install_hint: Option<String>,
}

pub fn materialize_acp_agent_contribution(
    plugin_name: &str,
    agent_id: &str,
    manifest: &AcpAgentManifest,
    root: &Path,
    source: &str,
    rebon_exe: &Path,
) -> anyhow::Result<PluginAcpAgentContribution> {
    let command = expand_placeholders(&manifest.command, root, rebon_exe)?;
    let args = manifest
        .args
        .iter()
        .map(|arg| expand_placeholders(arg, root, rebon_exe))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let env = manifest
        .env
        .iter()
        .map(|(key, value)| Ok((key.clone(), expand_placeholders(value, root, rebon_exe)?)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let cwd = manifest
        .cwd
        .as_deref()
        .map(|raw| materialize_cwd(raw, root, rebon_exe))
        .transpose()?;
    Ok(PluginAcpAgentContribution {
        id: agent_id.to_string(),
        plugin_name: plugin_name.to_string(),
        source: source.to_string(),
        display_name: manifest.display_name.clone(),
        command,
        args,
        env,
        cwd,
        inject_fs_tools: manifest.inject_fs_tools.unwrap_or(true),
        session_meta: manifest.session_meta.clone(),
        install_hint: manifest.install_hint.clone(),
    })
}

fn validate_env_key(key: &str) -> anyhow::Result<()> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        bail!("ACP agent env key must not be empty");
    }
    if trimmed != key {
        bail!("ACP agent env key `{key}` must not have leading/trailing whitespace");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        bail!("ACP agent env key `{key}` may only contain A-Z, 0-9 and _");
    }
    Ok(())
}

fn materialize_cwd(raw: &str, root: &Path, rebon_exe: &Path) -> anyhow::Result<PathBuf> {
    let expanded = expand_placeholders(raw, root, rebon_exe)?;
    let path = PathBuf::from(expanded);
    let candidate = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    ensure_path_inside_root(root, &candidate)?;
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> AcpAgentManifest {
        AcpAgentManifest {
            command: "{plugin_dir}/bin/agent".into(),
            args: vec!["--acp".into()],
            env: BTreeMap::from([("API_KEY".into(), "$MY_KEY".into())]),
            cwd: Some(".".into()),
            display_name: Some("Demo Agent".into()),
            inject_fs_tools: None,
            session_meta: Some(BTreeMap::from([(
                "claudeCode".into(),
                serde_json::json!({"options": {"disallowedTools": ["Write"]}}),
            )])),
            install_hint: Some("npm install -g demo-agent".into()),
        }
    }

    #[test]
    fn validates_and_materializes_an_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = manifest();
        manifest.validate("demo", Some(tmp.path()), &[]).unwrap();

        let contribution = materialize_acp_agent_contribution(
            "demo-plugin",
            "demo",
            &manifest,
            tmp.path(),
            "plugin:demo-plugin@user",
            Path::new("rebon"),
        )
        .unwrap();
        assert!(contribution.command.ends_with("agent"));
        assert!(!contribution.command.contains("{plugin_dir}"));
        assert_eq!(contribution.env["API_KEY"], "$MY_KEY");
        assert!(contribution.inject_fs_tools, "omitted means yes");
        assert_eq!(
            contribution.install_hint.as_deref(),
            Some("npm install -g demo-agent")
        );
    }

    #[test]
    fn rejects_the_local_sentinel_and_unlisted_bare_commands() {
        let mut m = manifest();
        assert!(m.validate("local", None, &[]).is_err());
        assert!(m.validate("Local", None, &[]).is_err());

        m.command = "gemini".into();
        assert!(
            m.validate("gemini", None, &[]).is_err(),
            "bare command must be listed in externalCommands"
        );
        m.validate("gemini", None, &["gemini".into()]).unwrap();
    }

    #[test]
    fn rejects_env_brace_indirection_and_bad_keys() {
        let mut m = manifest();
        m.env = BTreeMap::from([("API_KEY".into(), "${MY_KEY}".into())]);
        assert!(
            m.validate("demo", None, &[]).is_err(),
            "the brace form collides with plugin placeholders"
        );

        let mut m = manifest();
        m.env = BTreeMap::from([("lower".into(), "x".into())]);
        assert!(m.validate("demo", None, &[]).is_err());
    }

    #[test]
    fn cwd_cannot_escape_the_plugin_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = manifest();
        m.cwd = Some("../outside".into());
        assert!(m.validate("demo", Some(tmp.path()), &[]).is_err());
    }
}
