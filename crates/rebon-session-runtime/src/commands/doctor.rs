//! `/doctor`: this session's environment, checked and reported.
//!
//! The report is data -- sections of labelled rows, each with a status
//! -- so a worker can answer `/doctor` over IPC while a terminal draws
//! the same rows in a panel. Collecting it is this module's job; the
//! shapes it collects into, and the panel that shows them, belong to
//! `rebon_dialog::doctor_dialog`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rebon_types::wall_clock_ms;

use super::mcp::mcp_config_transport_label;
use super::SessionCommandInputs;
use crate::EngineSession;

pub use rebon_dialog::doctor_dialog::{DoctorItem, DoctorReport, DoctorSection, DoctorStatus};

pub fn execute_doctor_command(inputs: &SessionCommandInputs, session: &EngineSession) -> String {
    format_doctor_report(&collect_doctor_report(inputs, session))
}

pub fn collect_doctor_report(
    inputs: &SessionCommandInputs,
    session: &EngineSession,
) -> DoctorReport {
    use DoctorReport;

    let config_dir = doctor_config_home_dir();
    let cwd = Path::new(&session.cwd);
    let command_probes = DoctorCommandAvailability::runtime();
    // Off the `session-sandbox` seat, so a machine with the plugin switched
    // off says so in one line rather than reporting on a sandbox nothing is
    // going to build.
    let sandbox = rebon_harness::sandbox_doctor(cwd, &session.startup.settings);

    DoctorReport {
        summary: vec![
            ("Rebon version".into(), env!("CARGO_PKG_VERSION").into()),
            ("provider".into(), session.model.provider_name.clone()),
            ("model".into(), session.model.name.clone()),
            ("cwd".into(), session.cwd.clone()),
            ("session id".into(), session.session_id.clone()),
            (
                "permission mode".into(),
                inputs.permission_mode.as_wire().into(),
            ),
        ],
        sections: vec![
            doctor_section(
                "Provider / network",
                provider_doctor_rows(&config_dir, session, wall_clock_ms()),
            ),
            doctor_section(
                "Shell commands",
                shell_command_doctor_rows(
                    cfg!(target_os = "linux"),
                    sandbox.enabled_in_settings,
                    &command_probes,
                ),
            ),
            doctor_section("Config migration", config_doctor_rows(&config_dir)),
            doctor_section(
                "MCP servers",
                mcp_doctor_rows(
                    cwd,
                    &session.startup.mcp_configs,
                    session.startup.strict_mcp_config,
                ),
            ),
            doctor_section(
                "Sandbox",
                sandbox.lines.into_iter().map(DoctorRow::from).collect(),
            ),
        ],
    }
}

fn format_doctor_report(report: &DoctorReport) -> String {
    let summary = |label: &str| {
        report
            .summary
            .iter()
            .find(|(candidate, _)| candidate == label)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let sections = report
        .sections
        .iter()
        .map(
            |section| rebon_slash_commands::formatters::DoctorSectionDto {
                title: section.title.clone(),
                rows: section
                    .items
                    .iter()
                    .map(|item| rebon_slash_commands::formatters::DoctorRowDto {
                        status: match item.status {
                            DoctorStatus::Pass => {
                                rebon_slash_commands::formatters::DoctorStatus::Pass
                            }
                            DoctorStatus::Warn => {
                                rebon_slash_commands::formatters::DoctorStatus::Warn
                            }
                            DoctorStatus::Fail => {
                                rebon_slash_commands::formatters::DoctorStatus::Fail
                            }
                        },
                        label: item.label.clone(),
                        message: item.detail.clone(),
                        suggestion: Some(item.suggestion.clone()),
                    })
                    .collect(),
            },
        )
        .collect();
    rebon_slash_commands::formatters::format_doctor_command(
        rebon_slash_commands::formatters::DoctorCommandDto {
            version: summary("Rebon version"),
            provider: summary("provider"),
            model: summary("model"),
            cwd: summary("cwd"),
            session_id: summary("session id"),
            permission_mode: summary("permission mode"),
            sections,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorRowStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorRow {
    pub status: DoctorRowStatus,
    pub label: String,
    pub message: String,
}

impl DoctorRow {
    fn ok(label: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: DoctorRowStatus::Ok,
            label: label.into(),
            message: message.into(),
        }
    }

    fn warning(label: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: DoctorRowStatus::Warning,
            label: label.into(),
            message: message.into(),
        }
    }

    fn error(label: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: DoctorRowStatus::Error,
            label: label.into(),
            message: message.into(),
        }
    }
}

/// A diagnostic from a plugin, as a row this screen draws.
///
/// The plugin decides what is wrong with the machine and how bad it is; this
/// file decides what a row looks like. Neither has to know the other's type.
impl From<rebon_tool::DoctorLine> for DoctorRow {
    fn from(line: rebon_tool::DoctorLine) -> Self {
        Self {
            status: match line.level {
                rebon_tool::DoctorLevel::Ok => DoctorRowStatus::Ok,
                rebon_tool::DoctorLevel::Warning => DoctorRowStatus::Warning,
                rebon_tool::DoctorLevel::Error => DoctorRowStatus::Error,
            },
            label: line.label,
            message: line.message,
        }
    }
}

fn doctor_section(title: &str, rows: Vec<DoctorRow>) -> DoctorSection {
    let items = if rows.is_empty() {
        vec![DoctorItem {
            status: DoctorStatus::Pass,
            label: "none".into(),
            detail: "no findings".into(),
            suggestion: "No action is required.".into(),
        }]
    } else {
        rows.into_iter()
            .map(|row| {
                let status = match row.status {
                    DoctorRowStatus::Ok => DoctorStatus::Pass,
                    DoctorRowStatus::Warning => DoctorStatus::Warn,
                    DoctorRowStatus::Error => DoctorStatus::Fail,
                };
                let suggestion = match status {
                    DoctorStatus::Pass => "No action is required.".to_string(),
                    DoctorStatus::Warn => format!(
                        "Review the {} finding and apply its recommendation.",
                        row.label
                    ),
                    DoctorStatus::Fail => {
                        format!("Resolve the {} failure, then rerun /doctor.", row.label)
                    }
                };
                DoctorItem {
                    status,
                    label: row.label,
                    detail: row.message,
                    suggestion,
                }
            })
            .collect()
    };
    DoctorSection {
        title: title.into(),
        items,
    }
}

fn doctor_config_home_dir() -> PathBuf {
    if std::env::var_os("REBON_CONFIG_DIR").is_some_and(|value| !value.is_empty()) {
        return crate::rebon_config::config_home_dir();
    }
    let home = crate::rebon_config::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let canonical = home.join(crate::rebon_config::DEFAULT_CONFIG_DIR_NAME);
    if canonical.exists() {
        crate::rebon_config::config_home_dir()
    } else {
        canonical
    }
}

/// Which environment variable, if any, decided the config home.
///
/// Reported by `/doctor` because the failure it diagnoses is silent: a
/// variable set in one shell and not another sends a session to a different
/// data directory, and nothing about the UI says which one it landed on.
fn config_home_source() -> Option<&'static str> {
    rebon_session::config_home::CONFIG_HOME_VARS
        .into_iter()
        .find(|name| {
            std::env::var_os(name).is_some_and(|value| !value.to_string_lossy().trim().is_empty())
        })
}

pub fn provider_doctor_rows(
    config_dir: &Path,
    session: &EngineSession,
    now_ms: u64,
) -> Vec<DoctorRow> {
    let mut rows = Vec::new();
    rows.push(DoctorRow::ok(
        "runtime provider",
        format!(
            "session is using `{}` with model `{}`",
            session.model.provider_name, session.model.name
        ),
    ));

    let providers = crate::rebon_config::list_custom_providers_from(config_dir);
    let active = crate::rebon_config::get_active_custom_provider_name_from(config_dir);
    match crate::rebon_config::resolve_from_dir(config_dir) {
        Ok(Some(resolved)) => rows.push(DoctorRow::ok(
            "active provider resolution",
            format!("`{}` resolved as {:?}", resolved.name, resolved.format),
        )),
        Ok(None) => rows.push(DoctorRow::warning(
            "active provider resolution",
            "no active custom provider resolved; Rebon may be using environment fallback credentials",
        )),
        Err(err) => rows.push(DoctorRow::error(
            "active provider resolution",
            format!("failed to resolve active provider: {err}"),
        )),
    }

    if let Some(active_name) = active {
        rows.push(DoctorRow::ok(
            "active provider",
            format!("activeCustomProvider is `{active_name}`"),
        ));
        if let Some(provider) = providers
            .iter()
            .find(|provider| provider.name == active_name)
        {
            let format = provider.format.trim();
            if crate::rebon_config::VALID_PROVIDER_FORMATS.contains(&format) {
                rows.push(DoctorRow::ok("provider format", format.to_string()));
            } else {
                rows.push(DoctorRow::error(
                    "provider format",
                    format!(
                        "unsupported `{format}`; expected one of {}",
                        crate::rebon_config::VALID_PROVIDER_FORMATS.join(", ")
                    ),
                ));
            }

            if provider.model.trim().is_empty() {
                rows.push(DoctorRow::error(
                    "model",
                    "active provider has an empty model; set a model with /model or /provider",
                ));
            } else {
                rows.push(DoctorRow::ok("model", provider.model.clone()));
            }

            match validate_http_url(&provider.base_url) {
                Ok(()) => rows.push(DoctorRow::ok("base URL", provider.base_url.clone())),
                Err(err) => rows.push(DoctorRow::error(
                    "base URL",
                    format!("{} ({err})", provider.base_url),
                )),
            }

            append_provider_credential_rows(&mut rows, config_dir, provider, now_ms);
        } else {
            rows.push(DoctorRow::error(
                "active provider",
                format!("`{active_name}` is not present in customProviders[]"),
            ));
        }
    } else if providers.is_empty() {
        rows.push(DoctorRow::warning(
            "config provider",
            "no custom providers found in config.json",
        ));
        append_env_provider_rows(&mut rows);
    } else {
        rows.push(DoctorRow::warning(
            "active provider",
            format!(
                "customProviders[] contains {} entrie(s), but activeCustomProvider is not set",
                providers.len()
            ),
        ));
    }

    rows.push(DoctorRow::ok("live network probe", "skipped (local-only)"));
    rows
}

fn append_provider_credential_rows(
    rows: &mut Vec<DoctorRow>,
    config_dir: &Path,
    provider: &crate::rebon_config::CustomProviderInfo,
    now_ms: u64,
) {
    let api_key = provider.api_key.trim();
    if api_key.is_empty() {
        rows.push(DoctorRow::error(
            "credentials",
            "apiKey is empty; configure an API key or log in again",
        ));
        return;
    }

    let Some(login) = crate::rebon_config::account_login_for_api_key(api_key) else {
        rows.push(DoctorRow::ok(
            "credentials",
            "literal API key configured (value not shown)",
        ));
        return;
    };

    match crate::rebon_config::resolve_from_dir(config_dir) {
        Ok(Some(resolved)) => match resolved.oauth {
            Some(oauth) => {
                rows.push(DoctorRow::ok(
                    "credentials",
                    if login.is_codex() {
                        "OpenAI OAuth credentials loaded from .credentials.json".to_string()
                    } else {
                        format!("{} login loaded from .credentials.json", login.display_name)
                    },
                ));
                let has_refresh = oauth
                    .refresh_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty());
                let expired = crate::rebon_config::is_token_expired_at(now_ms, oauth.expires_at_ms);
                match (expired, has_refresh) {
                    (false, _) => rows.push(DoctorRow::ok(
                        "OAuth token",
                        "access token is not expired",
                    )),
                    (true, true) => rows.push(DoctorRow::warning(
                        "OAuth token",
                        "access token is expired or near expiry; refresh token exists, but refresh is skipped by local-only doctor",
                    )),
                    (true, false) => rows.push(DoctorRow::error(
                        "OAuth token",
                        "access token is expired or missing expiry and no refresh token is available; run /login",
                    )),
                }
            }
            None => rows.push(DoctorRow::error(
                "credentials",
                "provider uses OAuth sentinel but resolved provider did not include OAuth metadata",
            )),
        },
        Ok(None) => rows.push(DoctorRow::error(
            "credentials",
            "provider uses OAuth sentinel but no active provider resolved",
        )),
        Err(err) => rows.push(DoctorRow::error(
            "credentials",
            format!("OAuth credentials could not be loaded: {err}"),
        )),
    }
}

fn append_env_provider_rows(rows: &mut Vec<DoctorRow>) {
    let env_keys = ["ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY", "OPENAI_API_KEY"];
    let present = env_keys
        .iter()
        .copied()
        .filter(|key| std::env::var(key).is_ok_and(|value| !value.trim().is_empty()))
        .collect::<Vec<_>>();
    if present.is_empty() {
        rows.push(DoctorRow::warning(
            "environment credentials",
            "none of ANTHROPIC_API_KEY, DEEPSEEK_API_KEY, or OPENAI_API_KEY are set",
        ));
    } else {
        rows.push(DoctorRow::ok(
            "environment credentials",
            format!("{} set", present.join(", ")),
        ));
    }
}

fn validate_http_url(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("URL is empty".to_string());
    }
    let url = reqwest::Url::parse(trimmed).map_err(|err| err.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL must use http:// or https://".to_string());
    }
    if url.host_str().is_none() {
        return Err("URL must include a host".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorCommandAvailability {
    rg: Option<PathBuf>,
    bwrap: Option<PathBuf>,
    socat: Option<PathBuf>,
}

impl DoctorCommandAvailability {
    fn runtime() -> Self {
        Self {
            rg: crate::ripgrep::resolve_ripgrep_command()
                .ok()
                .map(|command| command.program),
            bwrap: resolve_command_on_path("bwrap"),
            socat: resolve_command_on_path("socat"),
        }
    }

    fn path_for(&self, command: &str) -> Option<&Path> {
        match command {
            "rg" => self.rg.as_deref(),
            "bwrap" => self.bwrap.as_deref(),
            "socat" => self.socat.as_deref(),
            _ => None,
        }
    }
}

/// `linux_host` rather than the sandbox's own platform enum: bubblewrap and
/// socat are Linux dependencies, that enum's answer here was always the
/// machine the binary was built for, and a plain bool is one fewer reason for
/// this file to know the sandbox exists. It stays a parameter so a test can
/// ask the Linux question from a Windows box.
pub fn shell_command_doctor_rows(
    linux_host: bool,
    sandbox_enabled: bool,
    commands: &DoctorCommandAvailability,
) -> Vec<DoctorRow> {
    let mut rows = Vec::new();
    rows.push(command_requirement_row(
        "rg",
        true,
        commands,
        crate::ripgrep::RIPGREP_ACTIONABLE_GUIDANCE,
    ));
    let linux_sandbox_deps_required = linux_host && sandbox_enabled;
    rows.push(command_requirement_row(
        "bwrap",
        linux_sandbox_deps_required,
        commands,
        "Install bubblewrap (for example: apt install bubblewrap) or disable sandbox.enabled.",
    ));
    rows.push(command_requirement_row(
        "socat",
        linux_sandbox_deps_required,
        commands,
        "Install socat (for example: apt install socat) or disable sandbox.enabled.",
    ));
    rows
}

fn command_requirement_row(
    command: &'static str,
    required: bool,
    commands: &DoctorCommandAvailability,
    missing_hint: &'static str,
) -> DoctorRow {
    match (required, commands.path_for(command)) {
        (true, Some(path)) => DoctorRow::ok(command, format!("found at {}", path.display())),
        (true, None) => DoctorRow::error(command, format!("not found. {missing_hint}")),
        (false, Some(path)) => DoctorRow::ok(
            command,
            format!(
                "found at {}; not required for current platform/settings",
                path.display()
            ),
        ),
        (false, None) => DoctorRow::ok(command, "not required for current platform/settings"),
    }
}

fn resolve_command_on_path(program: &str) -> Option<PathBuf> {
    resolve_command_with_env(
        program,
        std::env::var_os("PATH"),
        std::env::var_os("PATHEXT"),
    )
}

fn resolve_command_with_env(
    program: &str,
    path_env: Option<OsString>,
    pathext_env: Option<OsString>,
) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 || program_path.is_absolute() {
        return is_executable_file(program_path).then(|| program_path.to_path_buf());
    }
    let paths = path_env?;
    for dir in std::env::split_paths(&paths) {
        for candidate in command_path_candidates(&dir, program, pathext_env.as_ref()) {
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn command_path_candidates(
    dir: &Path,
    program: &str,
    pathext_env: Option<&OsString>,
) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        let program_path = Path::new(program);
        if program_path.extension().is_some() {
            candidates.push(dir.join(program));
            return candidates;
        }
        let pathext = pathext_env
            .cloned()
            .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
        for ext in pathext.to_string_lossy().split(';') {
            if ext.is_empty() {
                continue;
            }
            candidates.push(dir.join(format!("{program}{ext}")));
            candidates.push(dir.join(format!("{program}{}", ext.to_ascii_lowercase())));
        }
        candidates
    }

    #[cfg(not(windows))]
    {
        let _ = pathext_env;
        vec![dir.join(program)]
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn config_doctor_rows(config_dir: &Path) -> Vec<DoctorRow> {
    let mut rows = Vec::new();
    let config_path = crate::rebon_config::config_json_path(config_dir);
    rows.push(DoctorRow::ok(
        "config home",
        config_dir.display().to_string(),
    ));
    let primary_exists = match crate::rebon_config::check_primary_config_json(config_dir) {
        Ok(Some(())) => {
            rows.push(DoctorRow::ok(
                "canonical config.json",
                format!("{} parses as JSON", config_path.display()),
            ));
            true
        }
        Ok(None) => {
            rows.push(DoctorRow::warning(
                "canonical config.json",
                format!("{} does not exist", config_path.display()),
            ));
            false
        }
        Err(err) => {
            rows.push(DoctorRow::error(
                "canonical config.json",
                format!("{} is invalid: {}", err.file_path.display(), err.message),
            ));
            true
        }
    };

    if primary_exists {
        match crate::rebon_config::check_config_roundtrip_schema(config_dir) {
            Ok(Some(())) => rows.push(DoctorRow::ok(
                "round-trip schema",
                "known schema parsed and unknown top-level fields remain representable",
            )),
            Ok(None) => rows.push(DoctorRow::warning(
                "round-trip schema",
                "not checked because config.json is missing",
            )),
            Err(err) => rows.push(DoctorRow::error(
                "round-trip schema",
                format!("failed: {err}"),
            )),
        }
    } else {
        rows.push(DoctorRow::warning(
            "round-trip schema",
            "not checked because config.json is missing",
        ));
    }

    rows.push(match config_home_source() {
        Some(var) => DoctorRow::ok(
            "config home source",
            format!("${var} — data is read and written under it, not under ~/.rebon"),
        ),
        None => DoctorRow::ok(
            "config home source",
            "default ~/.rebon (set $REBON_CONFIG_DIR to keep the data elsewhere)",
        ),
    });

    rows
}

fn mcp_doctor_rows(
    cwd: &Path,
    runtime_mcp_configs: &[String],
    strict_mcp_config: bool,
) -> Vec<DoctorRow> {
    let configs = crate::mcp_config::collect_default_mcp_configs_with_overrides(
        cwd,
        runtime_mcp_configs,
        strict_mcp_config,
    );
    mcp_doctor_rows_from_configs(
        configs,
        std::env::var(crate::mcp_config::RETIRED_SERVERS_ENV)
            .ok()
            .as_deref(),
    )
}

/// `retired_env` is whatever the retired `REBON_MCP_SERVERS` variable still
/// holds. Nothing starts those servers any more, so a user whose shell profile
/// exports it would otherwise just find the servers missing with no
/// explanation.
pub fn mcp_doctor_rows_from_configs(
    configs: anyhow::Result<Vec<crate::mcp_config::CollectedMcpServerConfig>>,
    retired_env: Option<&str>,
) -> Vec<DoctorRow> {
    let mut rows = Vec::new();
    if retired_env
        .map(str::trim)
        .is_some_and(|raw| !raw.is_empty())
    {
        rows.push(DoctorRow::warning(
            crate::mcp_config::RETIRED_SERVERS_ENV,
            format!(
                "set but no longer read. Move each `name:command args` entry into \
                 `mcpServers` in ~/.rebon/config.json (or the project's .mcp.json) as \
                 {{\"name\": {{\"command\": \"…\", \"args\": [\"…\"]}}}}, then unset {}.",
                crate::mcp_config::RETIRED_SERVERS_ENV
            ),
        ));
    }
    match configs {
        Ok(configs) => {
            rows.push(DoctorRow::ok(
                "config parse",
                format!("{} configured server(s)", configs.len()),
            ));
            if configs.is_empty() {
                rows.push(DoctorRow::ok("servers", "none configured"));
            }
            for item in configs {
                rows.extend(mcp_server_doctor_rows(&item));
            }
        }
        Err(err) => rows.push(DoctorRow::error(
            "config parse",
            format!("failed to load MCP config: {err}"),
        )),
    }
    rows.push(DoctorRow::ok(
        "startup/connect probe",
        "skipped (local-only)",
    ));
    rows
}

fn mcp_server_doctor_rows(item: &crate::mcp_config::CollectedMcpServerConfig) -> Vec<DoctorRow> {
    let mut rows = Vec::new();
    let name = item.config.name();
    if name.trim().is_empty() {
        rows.push(DoctorRow::error("server name", "empty server name"));
    } else {
        rows.push(DoctorRow::ok(
            format!("server `{name}`"),
            format!(
                "transport `{}` from {}",
                mcp_config_transport_label(&item.config),
                item.source
            ),
        ));
    }

    match &item.config {
        crate::mcp_config::McpServerConfig::Stdio(config) => {
            rows.push(mcp_stdio_command_row(name, &config.command));
        }
        crate::mcp_config::McpServerConfig::Http(config) => {
            rows.push(mcp_url_row(name, "http", &config.url));
        }
        crate::mcp_config::McpServerConfig::Sse(config) => {
            rows.push(mcp_url_row(name, "sse", &config.url));
        }
    }
    rows
}

fn mcp_stdio_command_row(server_name: &str, command: &str) -> DoctorRow {
    let command = command.trim();
    if command.is_empty() {
        return DoctorRow::error(
            format!("MCP `{server_name}` command"),
            "stdio command is empty",
        );
    }
    let command_path = Path::new(command);
    if command_path.components().count() > 1 || command_path.is_absolute() {
        if is_executable_file(command_path) {
            DoctorRow::ok(
                format!("MCP `{server_name}` command"),
                format!("path exists and is executable: {command}"),
            )
        } else {
            DoctorRow::error(
                format!("MCP `{server_name}` command"),
                format!("path is not an executable file: {command}"),
            )
        }
    } else {
        DoctorRow::ok(
            format!("MCP `{server_name}` command"),
            format!("command name `{command}` configured; not executed"),
        )
    }
}

fn mcp_url_row(server_name: &str, transport: &str, url: &str) -> DoctorRow {
    match validate_http_url(url) {
        Ok(()) => DoctorRow::ok(
            format!("MCP `{server_name}` {transport} URL"),
            format!("{url}; not connected"),
        ),
        Err(err) => DoctorRow::error(
            format!("MCP `{server_name}` {transport} URL"),
            format!("{url} ({err})"),
        ),
    }
}
