use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use rebon_plugin_mcp::{HttpServerConfig, SseServerConfig, StdioServerConfig};
use serde::Deserialize;

const JSON_ENV_SOURCE: &str = "REBON_MCP_SERVERS_JSON";

/// The retired `name:command args;…` environment variable.
///
/// Nothing reads it any more. It survives as a name so `/doctor` can tell a
/// user whose shell profile still exports it that their servers are no longer
/// being started, and where to put them instead.
pub const RETIRED_SERVERS_ENV: &str = "REBON_MCP_SERVERS";

#[derive(Debug, Clone)]
pub struct CollectedMcpServerConfig {
    pub config: McpServerConfig,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMcpConfig {
    pub payload: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub enum McpServerConfig {
    Stdio(StdioServerConfig),
    Http(HttpServerConfig),
    Sse(SseServerConfig),
}

impl McpServerConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio(config) => &config.name,
            Self::Http(config) => &config.name,
            Self::Sse(config) => &config.name,
        }
    }
}

pub(crate) type CollectedStdioServerConfig = CollectedMcpServerConfig;

#[derive(Debug, Deserialize)]
struct McpConfigEnvelope {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, RawMcpServerConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMcpServerConfig {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(rename = "type")]
    transport_type: Option<String>,
    transport: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(rename = "headersHelper")]
    headers_helper: Option<String>,
    oauth: Option<serde_json::Value>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    /// `"disabled": true` parks a server without deleting it: it is skipped
    /// before it is read any further, so a parked entry that would not
    /// parse as a server cannot fail the rest.
    #[serde(default)]
    disabled: bool,
    /// `"shared": false` keeps a stdio server to one session: each session
    /// starts its own instead of using the one the shared host runs. For a
    /// server that keeps state per client.
    shared: Option<bool>,
}

impl RawMcpServerConfig {
    fn is_enabled(&self) -> bool {
        !self.disabled
    }
}

/// The MCP half of a project's `.rebon/settings.local.json`.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LocalMcpSettings {
    #[serde(default)]
    enable_all_project_mcp_servers: bool,
    #[serde(default)]
    enabled_mcpjson_servers: Vec<String>,
    #[serde(default)]
    disabled_mcpjson_servers: Vec<String>,
    /// Servers switched off in this project only, by name: a user-level
    /// (`config.json`) server and a `.mcp.json` one alike. The other
    /// projects keep them.
    #[serde(default)]
    disabled_mcp_servers: Vec<String>,
}

impl LocalMcpSettings {
    /// Whether this project switched `name` off for itself.
    fn turns_off(&self, name: &str) -> bool {
        self.disabled_mcp_servers
            .iter()
            .any(|disabled| disabled == name)
    }
}

/// Only `rebon-cli`'s tests name this; see the visibility rule in `crates/REBON.md`.
#[doc(hidden)]
pub fn collect_default_mcp_configs_with_overrides(
    cwd: &Path,
    mcp_configs: &[String],
    strict_mcp_config: bool,
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    collect_default_mcp_configs_with_plugin_overrides(cwd, mcp_configs, strict_mcp_config, &[])
}

pub fn collect_default_mcp_configs_with_plugin_overrides(
    cwd: &Path,
    mcp_configs: &[String],
    strict_mcp_config: bool,
    plugin_mcp_configs: &[PluginMcpConfig],
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    let config_path =
        rebon_config::paths::config_json_path(&rebon_config::paths::config_home_dir());
    collect_default_mcp_configs_with_plugin_overrides_from_user_config(
        cwd,
        mcp_configs,
        strict_mcp_config,
        plugin_mcp_configs,
        Some(&config_path),
    )
}

fn collect_default_mcp_configs_with_plugin_overrides_from_user_config(
    cwd: &Path,
    mcp_configs: &[String],
    strict_mcp_config: bool,
    plugin_mcp_configs: &[PluginMcpConfig],
    user_config_path: Option<&Path>,
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    let mut collected = Vec::new();
    let mut seen: HashMap<String, String> = HashMap::new();

    for item in parse_flag_mcp_configs(cwd, mcp_configs)? {
        push_unique(&mut collected, &mut seen, item)?;
    }

    for item in parse_plugin_mcp_configs(plugin_mcp_configs)? {
        push_plugin_unique(&mut collected, &mut seen, item)?;
    }

    if !strict_mcp_config {
        for item in collect_mcp_configs_from_sources_with_user_config(
            cwd,
            user_config_path,
            std::env::var(JSON_ENV_SOURCE).ok().as_deref(),
        )? {
            push_unique(&mut collected, &mut seen, item)?;
        }
    }

    Ok(collected)
}

#[cfg(test)]
pub(crate) fn collect_mcp_configs_from_sources(
    cwd: &Path,
    json_env: Option<&str>,
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    collect_mcp_configs_from_sources_with_user_config(cwd, None, json_env)
}

fn collect_mcp_configs_from_sources_with_user_config(
    cwd: &Path,
    user_config_path: Option<&Path>,
    json_env: Option<&str>,
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    let mut collected = Vec::new();
    let mut seen: HashMap<String, String> = HashMap::new();

    let user = match user_config_path {
        Some(path) => read_user_mcp_servers(path)?,
        None => None,
    };
    let project = read_project_mcp_servers(cwd)?;
    // Read only when there is a server it could approve or switch off, so a
    // project with no servers at all is not failed by its settings file.
    let settings = if user.is_some() || project.is_some() {
        load_local_mcp_settings(cwd)?
    } else {
        LocalMcpSettings::default()
    };

    if let Some(user) = user {
        for item in user.collect(|name| !settings.turns_off(name))? {
            push_unique(&mut collected, &mut seen, item)?;
        }
    }

    if let Some(raw) = json_env.map(str::trim).filter(|raw| !raw.is_empty()) {
        for item in parse_json_mcp_servers(raw, JSON_ENV_SOURCE)? {
            push_unique(&mut collected, &mut seen, item)?;
        }
    }

    if let Some(project) = project {
        for item in project.collect(|name| project_server_is_approved(name, &settings))? {
            push_unique(&mut collected, &mut seen, item)?;
        }
    }

    Ok(collected)
}

/// One file's `mcpServers`, read but not yet turned into servers.
struct ServerFile {
    source: String,
    servers: BTreeMap<String, RawMcpServerConfig>,
}

impl ServerFile {
    /// The entries that are on and that `wanted` keeps. Both are decided
    /// before an entry is read any further, so one that is off cannot fail
    /// the rest by not parsing.
    fn collect(
        self,
        wanted: impl Fn(&str) -> bool,
    ) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
        let source = self.source;
        self.servers
            .into_iter()
            .filter(|(name, raw_config)| raw_config.is_enabled() && wanted(name))
            .map(|(name, raw_config)| server_config_from_raw(name, raw_config, source.clone()))
            .collect()
    }
}

fn push_unique(
    collected: &mut Vec<CollectedStdioServerConfig>,
    seen: &mut HashMap<String, String>,
    item: CollectedStdioServerConfig,
) -> anyhow::Result<()> {
    if let Some(existing_source) = seen.get(item.config.name()) {
        return Err(anyhow!(
            "duplicate MCP server `{}` configured by both {} and {}",
            item.config.name(),
            existing_source,
            item.source
        ));
    }
    seen.insert(item.config.name().to_string(), item.source.clone());
    collected.push(item);
    Ok(())
}

fn push_plugin_unique(
    collected: &mut Vec<CollectedStdioServerConfig>,
    seen: &mut HashMap<String, String>,
    item: CollectedStdioServerConfig,
) -> anyhow::Result<()> {
    if let Some(existing_source) = seen.get(item.config.name()) {
        if existing_source.starts_with("--mcp-config[") {
            return Ok(());
        }
    }
    push_unique(collected, seen, item)
}

fn parse_json_mcp_servers(
    raw: &str,
    source: &str,
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .with_context(|| format!("failed to parse {source} as JSON MCP config"))?;
    let servers = parse_mcp_server_map_value(value, source)?;
    servers
        .into_iter()
        .filter(|(_, raw_config)| raw_config.is_enabled())
        .map(|(name, raw_config)| server_config_from_raw(name, raw_config, source.to_string()))
        .collect()
}

fn parse_flag_mcp_configs(
    cwd: &Path,
    mcp_configs: &[String],
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    let mut configs = Vec::new();
    for (index, raw) in mcp_configs.iter().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let source = format!("--mcp-config[{}]", index + 1);
        let payload = if trimmed.starts_with('{') || trimmed.starts_with('[') {
            trimmed.to_string()
        } else {
            let path = crate::rebon_config::resolve_against_cwd(cwd, trimmed);
            std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read MCP config {}", path.display()))?
        };
        configs.extend(parse_json_mcp_servers(&payload, &source)?);
    }
    Ok(configs)
}

/// `config.json`'s servers; `None` when it has no `mcpServers` or no file.
fn read_user_mcp_servers(path: &Path) -> anyhow::Result<Option<ServerFile>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse user config {}", path.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("user config {} must be a JSON object", path.display()))?;
    if !object.contains_key("mcpServers") {
        return Ok(None);
    }

    let source = format!("{}#mcpServers", path.display());
    let envelope: McpConfigEnvelope = serde_json::from_value(value)
        .with_context(|| format!("invalid MCP server schema in {source}"))?;
    Ok(Some(ServerFile {
        source,
        servers: envelope.mcp_servers,
    }))
}

fn parse_plugin_mcp_configs(
    plugin_mcp_configs: &[PluginMcpConfig],
) -> anyhow::Result<Vec<CollectedStdioServerConfig>> {
    let mut configs = Vec::new();
    for plugin in plugin_mcp_configs {
        configs.extend(parse_json_mcp_servers(&plugin.payload, &plugin.source)?);
    }
    Ok(configs)
}

fn parse_mcp_server_map_value(
    value: serde_json::Value,
    source: &str,
) -> anyhow::Result<BTreeMap<String, RawMcpServerConfig>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{source} must be a JSON object"))?;
    if object.contains_key("mcpServers") {
        let envelope: McpConfigEnvelope = serde_json::from_value(value)
            .with_context(|| format!("invalid MCP server schema in {source}"))?;
        Ok(envelope.mcp_servers)
    } else {
        serde_json::from_value(value)
            .with_context(|| format!("invalid bare MCP server object schema in {source}"))
    }
}

fn server_config_from_raw(
    name: String,
    raw: RawMcpServerConfig,
    source: String,
) -> anyhow::Result<CollectedMcpServerConfig> {
    if raw.headers_helper.is_some() {
        return Err(anyhow!(
            "unsupported MCP server `{name}` in {source}: `headersHelper` is not supported yet"
        ));
    }
    if raw.oauth.is_some() {
        return Err(anyhow!(
            "unsupported MCP server `{name}` in {source}: `oauth` is not supported yet"
        ));
    }

    let transport = raw.transport_type.as_deref().or(raw.transport.as_deref());
    let is_stdio = transport.is_none_or(|kind| kind.eq_ignore_ascii_case("stdio"));
    let is_http = transport.is_some_and(|kind| kind.eq_ignore_ascii_case("http"))
        || (transport.is_none() && raw.command.is_none() && raw.url.is_some());
    let is_sse = transport.is_some_and(|kind| kind.eq_ignore_ascii_case("sse"));

    if is_sse {
        let url = raw.url.ok_or_else(|| {
            anyhow!("invalid MCP SSE server `{name}` in {source}: `url` is required")
        })?;
        if url.trim().is_empty() {
            return Err(anyhow!(
                "invalid MCP SSE server `{name}` in {source}: `url` must not be empty"
            ));
        }
        let mut config = SseServerConfig::new(name, url);
        config.headers = raw.headers.into_iter().collect();
        config.request_timeout = raw.timeout_ms.map(Duration::from_millis);
        return Ok(CollectedMcpServerConfig {
            config: McpServerConfig::Sse(config),
            source,
        });
    }

    if is_http {
        let url = raw.url.ok_or_else(|| {
            anyhow!("invalid MCP HTTP server `{name}` in {source}: `url` is required")
        })?;
        if url.trim().is_empty() {
            return Err(anyhow!(
                "invalid MCP HTTP server `{name}` in {source}: `url` must not be empty"
            ));
        }
        let mut config = HttpServerConfig::new(name, url);
        config.headers = raw.headers.into_iter().collect();
        config.request_timeout = raw.timeout_ms.map(Duration::from_millis);
        return Ok(CollectedMcpServerConfig {
            config: McpServerConfig::Http(config),
            source,
        });
    }

    if !is_stdio {
        let kind = transport.unwrap_or("<missing>");
        return Err(anyhow!(
            "unsupported MCP server `{name}` in {source}: transport `{kind}` is not supported"
        ));
    }

    let command = raw.command.ok_or_else(|| {
        if raw.url.is_some() {
            anyhow!("invalid MCP server `{name}` in {source}: set `type: \"http\"` for URL-based MCP servers")
        } else {
            anyhow!("invalid MCP stdio server `{name}` in {source}: `command` is required")
        }
    })?;
    if command.trim().is_empty() {
        return Err(anyhow!(
            "invalid MCP server `{name}` in {source}: `command` must not be empty"
        ));
    }
    let mut config = StdioServerConfig::new(name, command, raw.args);
    config.env = raw.env.into_iter().collect();
    config.cwd = raw.cwd;
    config.request_timeout = raw.timeout_ms.map(Duration::from_millis);
    config.shared = raw.shared.unwrap_or(true);
    Ok(CollectedMcpServerConfig {
        config: McpServerConfig::Stdio(config),
        source,
    })
}

/// The project's `.mcp.json`; `None` when it has none.
fn read_project_mcp_servers(cwd: &Path) -> anyhow::Result<Option<ServerFile>> {
    let path = cwd.join(".mcp.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let project: McpConfigEnvelope = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse project MCP config {}", path.display()))?;
    Ok(Some(ServerFile {
        source: path.display().to_string(),
        servers: project.mcp_servers,
    }))
}

fn load_local_mcp_settings(cwd: &Path) -> anyhow::Result<LocalMcpSettings> {
    let path: PathBuf = cwd.join(".rebon").join("settings.local.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalMcpSettings::default());
        }
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to parse local MCP approval settings {}",
            path.display()
        )
    })
}

fn project_server_is_approved(name: &str, settings: &LocalMcpSettings) -> bool {
    if settings.turns_off(name)
        || settings
            .disabled_mcpjson_servers
            .iter()
            .any(|disabled| disabled == name)
    {
        return false;
    }
    settings.enable_all_project_mcp_servers
        || settings
            .enabled_mcpjson_servers
            .iter()
            .any(|enabled| enabled == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn collected_names(configs: &[CollectedMcpServerConfig]) -> Vec<String> {
        configs
            .iter()
            .map(|item| item.config.name().to_string())
            .collect()
    }

    fn as_stdio(config: &McpServerConfig) -> &StdioServerConfig {
        match config {
            McpServerConfig::Stdio(config) => config,
            McpServerConfig::Http(_) | McpServerConfig::Sse(_) => panic!("expected stdio config"),
        }
    }

    fn as_http(config: &McpServerConfig) -> &HttpServerConfig {
        match config {
            McpServerConfig::Http(config) => config,
            McpServerConfig::Stdio(_) | McpServerConfig::Sse(_) => panic!("expected http config"),
        }
    }

    fn as_sse(config: &McpServerConfig) -> &SseServerConfig {
        match config {
            McpServerConfig::Sse(config) => config,
            McpServerConfig::Stdio(_) | McpServerConfig::Http(_) => panic!("expected sse config"),
        }
    }

    #[test]
    fn json_env_supports_bare_server_map_object() {
        let temp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "bare": {
                "command": "node",
                "args": ["server.js"],
                "timeoutMs": 1234
            }
        }"#;

        let configs = collect_mcp_configs_from_sources(temp.path(), Some(raw)).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_stdio(&configs[0].config);
        assert_eq!(config.name, "bare");
        assert_eq!(config.command, "node");
        assert_eq!(config.args, vec!["server.js"]);
        assert_eq!(config.request_timeout, Some(Duration::from_millis(1234)));
        assert_eq!(configs[0].source, JSON_ENV_SOURCE);
    }

    #[test]
    fn stdio_servers_are_shared_unless_they_say_otherwise() {
        let temp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "mcpServers": {
                "plain": { "command": "node" },
                "own": { "command": "node", "shared": false },
                "said": { "command": "node", "shared": true }
            }
        }"#;
        let configs = collect_mcp_configs_from_sources(temp.path(), Some(raw)).unwrap();
        let shared: Vec<(String, bool)> = configs
            .iter()
            .map(|item| {
                let config = as_stdio(&item.config);
                (config.name.clone(), config.shared)
            })
            .collect();
        assert_eq!(
            shared,
            vec![
                ("own".to_string(), false),
                ("plain".to_string(), true),
                ("said".to_string(), true),
            ]
        );
    }

    #[test]
    fn json_env_supports_windows_command_path_and_args_with_spaces() {
        let temp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "mcpServers": {
                "win": {
                    "command": "C:\\Program Files\\nodejs\\node.exe",
                    "args": ["C:\\path with spaces\\server.js", "--flag with spaces"],
                    "env": {"KEY": "VALUE"},
                    "cwd": "C:\\work dir",
                    "timeoutMs": 30000
                }
            }
        }"#;

        let configs = collect_mcp_configs_from_sources(temp.path(), Some(raw)).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_stdio(&configs[0].config);
        assert_eq!(config.name, "win");
        assert_eq!(config.command, "C:\\Program Files\\nodejs\\node.exe");
        assert_eq!(
            config.args,
            vec!["C:\\path with spaces\\server.js", "--flag with spaces"]
        );
        assert_eq!(config.env, vec![("KEY".to_string(), "VALUE".to_string())]);
        assert_eq!(config.cwd.as_deref(), Some("C:\\work dir"));
        assert_eq!(config.request_timeout, Some(Duration::from_millis(30000)));
    }

    #[test]
    fn user_config_loads_stdio_and_http_servers_without_treating_other_keys_as_servers() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(
            &config_path,
            r#"{
                "theme": "light",
                "mcpServers": {
                    "netcatty": {
                        "command": "C:\\Program Files\\Netcatty\\netcatty-external-mcp.cmd",
                        "args": ["--stdio"],
                        "env": {"MODE": "external"}
                    },
                    "remote": {
                        "type": "http",
                        "url": "https://example.test/mcp"
                    }
                }
            }"#,
        );

        let configs = collect_mcp_configs_from_sources_with_user_config(
            temp.path(),
            Some(&config_path),
            None,
        )
        .unwrap();

        assert_eq!(collected_names(&configs), vec!["netcatty", "remote"]);
        let stdio = as_stdio(&configs[0].config);
        assert_eq!(
            stdio.command,
            "C:\\Program Files\\Netcatty\\netcatty-external-mcp.cmd"
        );
        assert_eq!(stdio.args, vec!["--stdio"]);
        assert_eq!(stdio.env, vec![("MODE".into(), "external".into())]);
        assert_eq!(as_http(&configs[1].config).url, "https://example.test/mcp");
        assert!(configs
            .iter()
            .all(|item| item.source.ends_with("config.json#mcpServers")));
    }

    #[test]
    fn user_config_skips_disabled_servers_and_keeps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(
            &config_path,
            r#"{
                "mcpServers": {
                    "parked": {"command": "node", "disabled": true},
                    "explicit-on": {"command": "node", "disabled": false},
                    "on": {"type": "http", "url": "https://example.test/mcp"}
                }
            }"#,
        );

        let configs = collect_mcp_configs_from_sources_with_user_config(
            temp.path(),
            Some(&config_path),
            None,
        )
        .unwrap();

        assert_eq!(collected_names(&configs), vec!["explicit-on", "on"]);
    }

    #[test]
    fn a_disabled_server_that_would_not_parse_does_not_fail_the_others() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(
            &config_path,
            r#"{
                "mcpServers": {
                    "no-command": {"args": ["x"], "disabled": true},
                    "oauth": {"type": "http", "url": "https://a.test", "oauth": {}, "disabled": true},
                    "websocket": {"type": "ws", "url": "ws://a.test", "disabled": true},
                    "fine": {"command": "node"}
                }
            }"#,
        );

        let configs = collect_mcp_configs_from_sources_with_user_config(
            temp.path(),
            Some(&config_path),
            None,
        )
        .unwrap();

        assert_eq!(collected_names(&configs), vec!["fine"]);
    }

    #[test]
    fn a_disabled_flag_of_the_wrong_type_is_still_a_schema_error() {
        let temp = tempfile::tempdir().unwrap();
        let raw = r#"{"mcpServers": {"x": {"command": "node", "disabled": "yes"}}}"#;
        let error = collect_mcp_configs_from_sources(temp.path(), Some(raw)).unwrap_err();
        assert!(format!("{error:#}").contains("invalid MCP server schema"));
    }

    #[test]
    fn json_env_and_project_servers_honor_disabled_too() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {
                "project-on": {"command": "node"},
                "project-off": {"command": "node", "disabled": true}
            }}"#,
        );
        write_file(
            &temp.path().join(".rebon").join("settings.local.json"),
            r#"{"enableAllProjectMcpServers":true}"#,
        );
        let raw =
            r#"{"env-on": {"command": "node"}, "env-off": {"command": "node", "disabled": true}}"#;

        let configs = collect_mcp_configs_from_sources(temp.path(), Some(raw)).unwrap();

        assert_eq!(collected_names(&configs), vec!["env-on", "project-on"]);
    }

    #[test]
    fn user_config_without_mcp_servers_and_missing_user_config_are_noops() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(&config_path, r#"{"theme":"light","providers":{}}"#);

        let without_section = collect_mcp_configs_from_sources_with_user_config(
            temp.path(),
            Some(&config_path),
            None,
        )
        .unwrap();
        let missing = collect_mcp_configs_from_sources_with_user_config(
            temp.path(),
            Some(&temp.path().join("missing.json")),
            None,
        )
        .unwrap();

        assert!(without_section.is_empty());
        assert!(missing.is_empty());
    }

    #[test]
    fn invalid_user_mcp_schema_reports_config_path_and_section() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(
            &config_path,
            r#"{"mcpServers":{"bad":{"args":"not-an-array"}}}"#,
        );

        let err = collect_mcp_configs_from_sources_with_user_config(
            temp.path(),
            Some(&config_path),
            None,
        )
        .unwrap_err();
        let message = format!("{err:#}");

        assert!(message.contains("invalid MCP server schema"));
        assert!(message.contains("config.json#mcpServers"));
        assert!(message.contains("invalid type") || message.contains("expected a sequence"));
    }

    #[test]
    fn duplicate_server_name_across_user_config_and_json_env_errors() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(
            &config_path,
            r#"{"mcpServers":{"dupe":{"command":"user-command"}}}"#,
        );
        let json_env = r#"{"mcpServers":{"dupe":{"command":"env-command"}}}"#;

        let err = collect_mcp_configs_from_sources_with_user_config(
            temp.path(),
            Some(&config_path),
            Some(json_env),
        )
        .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("duplicate MCP server `dupe`"));
        assert!(message.contains("config.json#mcpServers"));
        assert!(message.contains(JSON_ENV_SOURCE));
    }

    #[test]
    fn strict_mode_excludes_user_config() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(
            &config_path,
            r#"{"mcpServers":{"user":{"command":"user-command"}}}"#,
        );
        let flag = r#"{"mcpServers":{"flag":{"command":"flag-command"}}}"#;

        let configs = collect_default_mcp_configs_with_plugin_overrides_from_user_config(
            temp.path(),
            &[flag.to_string()],
            true,
            &[],
            Some(&config_path),
        )
        .unwrap();

        assert_eq!(collected_names(&configs), vec!["flag"]);
    }

    #[test]
    fn flag_mcp_config_supports_strict_mode() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"project":{"command":"node","args":["project.js"]}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enableAllProjectMcpServers":true}"#,
        );
        let raw = r#"{"mcpServers":{"flagged":{"command":"python","args":["server.py"]}}}"#;

        let configs =
            collect_default_mcp_configs_with_overrides(temp.path(), &[raw.to_string()], true)
                .unwrap();

        assert_eq!(collected_names(&configs), vec!["flagged"]);
        assert_eq!(configs[0].source, "--mcp-config[1]");
    }

    #[test]
    fn flag_mcp_config_shadows_duplicate_plugin_mcp_server() {
        let temp = tempfile::tempdir().unwrap();
        let raw =
            r#"{"mcpServers":{"rust_lsp":{"command":"flag-rebon","args":["lsp-mcp","rust"]}}}"#;
        let plugin = PluginMcpConfig {
            payload: r#"{"mcpServers":{"rust_lsp":{"command":"plugin-rebon","args":["lsp-mcp","rust"]}}}"#
                .to_string(),
            source: "plugin:rust-lsp@builtin:user".to_string(),
        };

        let configs = collect_default_mcp_configs_with_plugin_overrides_from_user_config(
            temp.path(),
            &[raw.to_string()],
            false,
            &[plugin],
            Some(&temp.path().join("config.json")),
        )
        .unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_stdio(&configs[0].config);
        assert_eq!(config.name, "rust_lsp");
        assert_eq!(config.command, "flag-rebon");
        assert_eq!(config.args, vec!["lsp-mcp", "rust"]);
        assert_eq!(configs[0].source, "--mcp-config[1]");
    }

    #[test]
    fn enable_all_project_mcp_servers_loads_all_except_disabled() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "alpha": {"command": "node", "args": ["alpha.js"]},
                    "beta": {"command": "python", "args": ["beta.py"]},
                    "disabled": {"command": "ruby", "args": ["disabled.rb"]}
                }
            }"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{
                "enableAllProjectMcpServers": true,
                "disabledMcpjsonServers": ["disabled"]
            }"#,
        );

        let configs = collect_mcp_configs_from_sources(temp.path(), None).unwrap();

        assert_eq!(collected_names(&configs), vec!["alpha", "beta"]);
        assert!(configs.iter().all(|item| item.source.contains(".mcp.json")));
    }

    #[test]
    fn disabled_project_mcp_server_overrides_explicit_enabled() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "conflict": {"command": "node", "args": ["conflict.js"]},
                    "approved": {"command": "node", "args": ["approved.js"]}
                }
            }"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{
                "enabledMcpjsonServers": ["conflict", "approved"],
                "disabledMcpjsonServers": ["conflict"]
            }"#,
        );

        let configs = collect_mcp_configs_from_sources(temp.path(), None).unwrap();

        assert_eq!(collected_names(&configs), vec!["approved"]);
    }

    #[test]
    fn approved_project_mcp_json_server_is_collected_disabled_server_is_not() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "approved": {"command": "node", "args": ["server.js"]},
                    "disabled": {"command": "python", "args": ["server.py"]},
                    "pending": {"command": "ruby", "args": ["server.rb"]}
                }
            }"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{
                "enabledMcpjsonServers": ["approved"],
                "disabledMcpjsonServers": ["disabled"]
            }"#,
        );

        let configs = collect_mcp_configs_from_sources(temp.path(), None).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_stdio(&configs[0].config);
        assert_eq!(config.name, "approved");
        assert_eq!(config.command, "node");
        assert_eq!(config.args, vec!["server.js"]);
    }

    #[test]
    fn malformed_project_mcp_json_errors_instead_of_noop() {
        let temp = tempfile::tempdir().unwrap();
        write_file(&temp.path().join(".mcp.json"), "{ not json");

        let err = collect_mcp_configs_from_sources(temp.path(), None).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("failed to parse project MCP config"));
        assert!(message.contains(".mcp.json"));
    }

    #[test]
    fn duplicate_server_name_across_json_env_and_project_config_errors() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"dupe": {"command": "project-cmd"}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["dupe"]}"#,
        );
        let json_env = r#"{"mcpServers": {"dupe": {"command": "env-cmd"}}}"#;

        let err = collect_mcp_configs_from_sources(temp.path(), Some(json_env)).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("duplicate MCP server `dupe`"));
        assert!(message.contains(JSON_ENV_SOURCE));
        assert!(message.contains(".mcp.json"));
    }

    #[test]
    fn approved_project_http_server_is_collected() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"patent-search": {"type": "http", "url": "http://127.0.0.1:3010/mcp"}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["patent-search"]}"#,
        );

        let configs = collect_mcp_configs_from_sources(temp.path(), None).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_http(&configs[0].config);
        assert_eq!(config.name, "patent-search");
        assert_eq!(config.url, "http://127.0.0.1:3010/mcp");
        assert!(config.headers.is_empty());
        assert!(configs[0].source.contains(".mcp.json"));
    }

    #[test]
    fn approved_project_url_only_server_is_inferred_as_http() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"patent-search": {"url": "http://127.0.0.1:3010/mcp"}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["patent-search"]}"#,
        );

        let configs = collect_mcp_configs_from_sources(temp.path(), None).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_http(&configs[0].config);
        assert_eq!(config.name, "patent-search");
        assert_eq!(config.url, "http://127.0.0.1:3010/mcp");
        assert!(config.headers.is_empty());
        assert!(configs[0].source.contains(".mcp.json"));
    }

    #[test]
    fn approved_project_http_server_preserves_headers_and_timeout() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "remote": {
                        "type": "http",
                        "url": "https://example.test/mcp",
                        "headers": {"Authorization": "Bearer token", "X-Trace": "abc"},
                        "timeoutMs": 4500
                    }
                }
            }"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["remote"]}"#,
        );

        let configs = collect_mcp_configs_from_sources(temp.path(), None).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_http(&configs[0].config);
        assert_eq!(config.name, "remote");
        assert_eq!(config.url, "https://example.test/mcp");
        assert_eq!(
            config.headers,
            vec![
                ("Authorization".to_string(), "Bearer token".to_string()),
                ("X-Trace".to_string(), "abc".to_string())
            ]
        );
        assert_eq!(config.request_timeout, Some(Duration::from_millis(4500)));
    }

    #[test]
    fn json_env_http_server_preserves_headers_and_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "mcpServers": {
                "remote": {
                    "type": "http",
                    "url": "https://example.test/mcp",
                    "headers": {"Authorization": "Bearer token", "X-Trace": "abc"},
                    "timeoutMs": 4500
                }
            }
        }"#;

        let configs = collect_mcp_configs_from_sources(temp.path(), Some(raw)).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_http(&configs[0].config);
        assert_eq!(config.name, "remote");
        assert_eq!(config.url, "https://example.test/mcp");
        assert_eq!(
            config.headers,
            vec![
                ("Authorization".to_string(), "Bearer token".to_string()),
                ("X-Trace".to_string(), "abc".to_string())
            ]
        );
        assert_eq!(config.request_timeout, Some(Duration::from_millis(4500)));
        assert_eq!(configs[0].source, JSON_ENV_SOURCE);
    }

    #[test]
    fn transport_http_is_supported_as_type_http_alias() {
        let temp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "mcpServers": {
                "remote": {"transport": "http", "url": "https://example.test/mcp"}
            }
        }"#;

        let configs = collect_mcp_configs_from_sources(temp.path(), Some(raw)).unwrap();

        assert_eq!(configs.len(), 1);
        let config = as_http(&configs[0].config);
        assert_eq!(config.name, "remote");
        assert_eq!(config.url, "https://example.test/mcp");
    }

    #[test]
    fn headers_helper_and_oauth_error_with_name_source_and_field() {
        let temp = tempfile::tempdir().unwrap();
        let headers_helper = r#"{
            "mcpServers": {
                "remote": {
                    "type": "http",
                    "url": "https://example.test/mcp",
                    "headersHelper": "helper-command"
                }
            }
        }"#;
        let err = collect_mcp_configs_from_sources(temp.path(), Some(headers_helper)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unsupported MCP server `remote`"));
        assert!(message.contains(JSON_ENV_SOURCE));
        assert!(message.contains("`headersHelper` is not supported yet"));

        let oauth = r#"{
            "mcpServers": {
                "authz": {
                    "type": "http",
                    "url": "https://example.test/mcp",
                    "oauth": {"clientId": "abc"}
                }
            }
        }"#;
        let err = collect_mcp_configs_from_sources(temp.path(), Some(oauth)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unsupported MCP server `authz`"));
        assert!(message.contains(JSON_ENV_SOURCE));
        assert!(message.contains("`oauth` is not supported yet"));
    }

    #[test]
    fn sse_type_and_transport_parse_as_legacy_sse() {
        let temp = tempfile::tempdir().unwrap();
        for (field, value) in [("type", "sse"), ("transport", "sse")] {
            let raw = format!(
                r#"{{"mcpServers": {{"legacy-sse": {{"{field}": "{value}", "url": "https://example.test/sse", "headers": {{"Authorization": "Bearer token"}}, "timeoutMs": 2500}}}}}}"#
            );

            let configs = collect_mcp_configs_from_sources(temp.path(), Some(&raw)).unwrap();
            assert_eq!(configs.len(), 1);
            let config = as_sse(&configs[0].config);
            assert_eq!(config.name, "legacy-sse");
            assert_eq!(config.url, "https://example.test/sse");
            assert_eq!(
                config.headers,
                vec![("Authorization".to_string(), "Bearer token".to_string())]
            );
            assert_eq!(config.request_timeout, Some(Duration::from_millis(2500)));
            assert_eq!(configs[0].source, JSON_ENV_SOURCE);
        }
    }

    #[test]
    fn empty_http_url_and_stdio_command_errors_are_pinned() {
        let temp = tempfile::tempdir().unwrap();
        let empty_url = r#"{"mcpServers": {"remote": {"type": "http", "url": "   "}}}"#;
        let err = collect_mcp_configs_from_sources(temp.path(), Some(empty_url)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("invalid MCP HTTP server `remote`"));
        assert!(message.contains(JSON_ENV_SOURCE));
        assert!(message.contains("`url` must not be empty"));

        let empty_command = r#"{"mcpServers": {"local": {"command": "   "}}}"#;
        let err = collect_mcp_configs_from_sources(temp.path(), Some(empty_command)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("invalid MCP server `local`"));
        assert!(message.contains(JSON_ENV_SOURCE));
        assert!(message.contains("`command` must not be empty"));
    }

    #[test]
    fn duplicate_server_name_across_mixed_http_and_stdio_sources_errors() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"dupe": {"command": "node", "args": ["server.js"]}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["dupe"]}"#,
        );
        let json_env = r#"{
            "mcpServers": {
                "dupe": {"type": "http", "url": "https://example.test/mcp"}
            }
        }"#;

        let err = collect_mcp_configs_from_sources(temp.path(), Some(json_env)).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("duplicate MCP server `dupe`"));
        assert!(message.contains(JSON_ENV_SOURCE));
        assert!(message.contains(".mcp.json"));
    }

    #[test]
    fn approved_project_http_server_missing_url_errors_clearly() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"remote": {"type": "http"}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["remote"]}"#,
        );

        let err = collect_mcp_configs_from_sources(temp.path(), None).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("invalid MCP HTTP server `remote`"));
        assert!(message.contains("`url` is required"));
        assert!(message.contains(".mcp.json"));
    }

    #[test]
    fn invalid_approved_project_args_schema_errors_clearly() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"bad": {"command": "node", "args": "server.js"}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["bad"]}"#,
        );

        let err = collect_mcp_configs_from_sources(temp.path(), None).unwrap_err();
        let message = format!("{err:#}");

        assert!(message.contains("failed to parse project MCP config"));
        assert!(message.contains(".mcp.json"));
        assert!(message.contains("invalid type") || message.contains("expected a sequence"));
    }

    #[test]
    fn invalid_approved_project_timeout_schema_errors_clearly() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"bad": {"command": "node", "timeoutMs": "slow"}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enabledMcpjsonServers": ["bad"]}"#,
        );

        let err = collect_mcp_configs_from_sources(temp.path(), None).unwrap_err();
        let message = format!("{err:#}");

        assert!(message.contains("failed to parse project MCP config"));
        assert!(message.contains(".mcp.json"));
        assert!(message.contains("invalid type") || message.contains("expected u64"));
    }

    /// A `config.json` with two user servers, and a project directory.
    fn user_servers_and_project() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("home").join("config.json");
        write_file(
            &config_path,
            r#"{"mcpServers": {
                "fs": {"command": "node"},
                "docs": {"type": "http", "url": "https://example.test/mcp"}
            }}"#,
        );
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        (temp, config_path, project)
    }

    fn collect_in(project: &Path, config_path: &Path) -> anyhow::Result<Vec<String>> {
        collect_mcp_configs_from_sources_with_user_config(project, Some(config_path), None)
            .map(|configs| collected_names(&configs))
    }

    #[test]
    fn a_project_can_switch_off_a_user_server_for_itself_only() {
        let (temp, config_path, project) = user_servers_and_project();
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        write_file(
            &project.join(".rebon").join("settings.local.json"),
            r#"{"disabledMcpServers": ["fs"]}"#,
        );

        assert_eq!(collect_in(&project, &config_path).unwrap(), vec!["docs"]);
        assert_eq!(
            collect_in(&other, &config_path).unwrap(),
            vec!["docs", "fs"],
            "another project keeps it"
        );
    }

    #[test]
    fn switching_a_project_server_off_beats_every_approval() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {
                "listed": {"command": "node"},
                "all": {"command": "node"},
                "kept": {"command": "node"}
            }}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{
                "enableAllProjectMcpServers": true,
                "enabledMcpjsonServers": ["listed"],
                "disabledMcpServers": ["listed", "all"]
            }"#,
        );

        let configs = collect_mcp_configs_from_sources(temp.path(), None).unwrap();

        assert_eq!(collected_names(&configs), vec!["kept"]);
    }

    #[test]
    fn a_user_server_switched_off_here_is_not_read_any_further() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.json");
        write_file(
            &config_path,
            r#"{"mcpServers": {
                "broken": {"command": "node", "headersHelper": "x"},
                "fine": {"command": "node"}
            }}"#,
        );
        assert!(
            collect_in(temp.path(), &config_path).is_err(),
            "the broken entry fails the load while it is on"
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"disabledMcpServers": ["broken"]}"#,
        );

        assert_eq!(collect_in(temp.path(), &config_path).unwrap(), vec!["fine"]);
    }

    #[test]
    fn the_project_switch_leaves_servers_given_on_purpose_alone() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"disabledMcpServers": ["env-server", "flag-server"]}"#,
        );
        let env = r#"{"env-server": {"command": "node"}}"#;
        let configs = collect_mcp_configs_from_sources(temp.path(), Some(env)).unwrap();
        assert_eq!(collected_names(&configs), vec!["env-server"]);

        // Not strict, so the project's settings are in play.
        let _home = rebon_tool::tasks::test_support::TestConfigHome::new("mcp-flag-scope");
        let flag = r#"{"mcpServers": {"flag-server": {"command": "node"}}}"#.to_string();
        let configs =
            collect_default_mcp_configs_with_overrides(temp.path(), &[flag], false).unwrap();
        assert_eq!(collected_names(&configs), vec!["flag-server"]);
    }

    #[test]
    fn the_settings_file_is_read_only_when_a_server_could_depend_on_it() {
        let (temp, config_path, project) = user_servers_and_project();
        write_file(
            &project.join(".rebon/settings.local.json"),
            r#"{"disabledMcpServers": "fs"}"#,
        );
        let nothing = temp.path().join("nothing.json");
        write_file(&nothing, r#"{"theme": "dark"}"#);

        assert_eq!(
            collect_in(&project, &nothing).unwrap(),
            Vec::<String>::new(),
            "no server anywhere: the broken file decides nothing"
        );
        let error = collect_in(&project, &config_path).unwrap_err();
        assert!(
            format!("{error:#}").contains("settings.local.json"),
            "with servers to decide about, a broken file is not skipped: {error:#}"
        );
    }

    #[test]
    fn invalid_local_settings_schema_errors_clearly() {
        let temp = tempfile::tempdir().unwrap();
        write_file(
            &temp.path().join(".mcp.json"),
            r#"{"mcpServers": {"approved": {"command": "node"}}}"#,
        );
        write_file(
            &temp.path().join(".rebon/settings.local.json"),
            r#"{"enableAllProjectMcpServers": "yes"}"#,
        );

        let err = collect_mcp_configs_from_sources(temp.path(), None).unwrap_err();
        let message = format!("{err:#}");

        assert!(message.contains("failed to parse local MCP approval settings"));
        assert!(message.contains("settings.local.json"));
        assert!(message.contains("invalid type"));
    }
}
