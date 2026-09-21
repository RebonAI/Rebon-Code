use std::path::PathBuf;
use std::str::FromStr;

use rebon_dialog::invalid_settings::ValidationErrorInput;
use serde::{Deserialize, Serialize};

pub use crate::rebon_config::MathRenderingMode;
/// Where the terminal's own shape is decided is a persisted setting, so the
/// enum lives with the other ones in `rebon-config`. Re-exported here because
/// this is the module the binary reaches for it through, and because the
/// setup wizard — a plugin, which cannot depend on the binary — needs the
/// same type for its UI-mode step.
pub use crate::rebon_config::UiMode;

/// Snapshot of the small local-settings subset used by preflight flows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalSettingsSnapshot {
    pub enable_all_project_mcp_servers: bool,
    pub enabled_mcpjson_servers: Vec<String>,
    pub disabled_mcpjson_servers: Vec<String>,
}

/// Result of scanning settings files before TUI startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsScan {
    pub errors: Vec<ValidationErrorInput>,
    pub local_settings: LocalSettingsSnapshot,
    pub user_ui: Option<UiSettingsSource>,
    pub project_ui: Option<UiSettingsSource>,
    pub flag_ui: Option<UiSettingsSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InlineUiSettings {
    pub viewport_height: u16,
    pub commit_tool_output: InlineCommitToolOutput,
    pub stream_flush: InlineStreamFlushSettings,
}

impl Default for InlineUiSettings {
    fn default() -> Self {
        Self {
            viewport_height: 12,
            commit_tool_output: InlineCommitToolOutput::Compact,
            stream_flush: InlineStreamFlushSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InlineCommitToolOutput {
    Compact,
}

impl Default for InlineCommitToolOutput {
    fn default() -> Self {
        Self::Compact
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InlineStreamFlushSettings {
    pub max_held_tool_uses: usize,
    pub max_held_lines: u16,
    pub max_held_ms: u64,
}

impl Default for InlineStreamFlushSettings {
    fn default() -> Self {
        Self {
            max_held_tool_uses: 6,
            max_held_lines: 12,
            max_held_ms: 500,
        }
    }
}

pub const STATUS_LINE_MAX_PADDING: u32 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUiConfig {
    pub mode: UiMode,
    pub math_rendering: MathRenderingMode,
    pub inline: InlineUiSettings,
    pub status_line: Option<StatusLineConfig>,
}

impl Default for ResolvedUiConfig {
    fn default() -> Self {
        Self {
            mode: UiMode::Screen,
            math_rendering: MathRenderingMode::Off,
            inline: InlineUiSettings::default(),
            status_line: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLineConfig {
    #[serde(rename = "type")]
    pub kind: StatusLineKind,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub script: Option<PathBuf>,
    #[serde(default)]
    pub padding: Option<u32>,
    #[serde(default, rename = "refreshInterval")]
    pub refresh_interval: Option<u64>,
    #[serde(default, rename = "hideVimModeIndicator")]
    pub hide_vim_mode_indicator: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusLineKind {
    Command,
    Script,
}

impl StatusLineConfig {
    pub fn source_label(&self) -> (&'static str, String) {
        match self.kind {
            StatusLineKind::Command => ("command", self.command.clone()),
            StatusLineKind::Script => (
                "script",
                self.script
                    .as_deref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        }
    }

    pub fn padding(&self) -> u32 {
        self.padding.unwrap_or(0)
    }

    pub fn refresh_interval_secs(&self) -> u64 {
        self.refresh_interval.unwrap_or(1).max(1)
    }

    pub fn hide_vim_mode_indicator(&self) -> bool {
        self.hide_vim_mode_indicator.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiSettingsSource {
    pub ui_mode: Option<UiMode>,
    pub math_rendering: Option<MathRenderingMode>,
    pub inline: Option<InlineUiSettings>,
    pub status_line: Option<StatusLineConfig>,
}

pub(crate) fn resolve_ui_config(
    cli_ui_mode: Option<UiMode>,
    env_ui_mode: Option<&str>,
    user: Option<&UiSettingsSource>,
    project: Option<&UiSettingsSource>,
    flag: Option<&UiSettingsSource>,
) -> anyhow::Result<ResolvedUiConfig> {
    let env_mode = env_ui_mode
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(<UiMode as FromStr>::from_str)
        .transpose()
        .map_err(anyhow::Error::msg)?;

    let mode = cli_ui_mode
        .or(env_mode)
        .or_else(|| flag.and_then(|s| s.ui_mode))
        .or_else(|| project.and_then(|s| s.ui_mode))
        .or_else(|| user.and_then(|s| s.ui_mode))
        .unwrap_or_default();

    let math_rendering = flag
        .and_then(|s| s.math_rendering)
        .or_else(|| project.and_then(|s| s.math_rendering))
        .or_else(|| user.and_then(|s| s.math_rendering))
        .unwrap_or_default();

    let mut inline = InlineUiSettings::default();
    if let Some(user_inline) = user.and_then(|s| s.inline.clone()) {
        inline = user_inline;
    }
    if let Some(project_inline) = project.and_then(|s| s.inline.clone()) {
        inline = project_inline;
    }
    if let Some(flag_inline) = flag.and_then(|s| s.inline.clone()) {
        inline = flag_inline;
    }
    validate_inline_settings(&inline)?;

    let status_line = flag
        .and_then(|s| s.status_line.clone())
        .or_else(|| project.and_then(|s| s.status_line.clone()))
        .or_else(|| user.and_then(|s| s.status_line.clone()));
    if let Some(status_line) = &status_line {
        validate_status_line_config(status_line)?;
    }

    Ok(ResolvedUiConfig {
        mode,
        math_rendering,
        inline,
        status_line,
    })
}

pub fn resolve_from_scan(
    cli_ui_mode: Option<UiMode>,
    scan: &SettingsScan,
) -> anyhow::Result<ResolvedUiConfig> {
    resolve_ui_config(
        cli_ui_mode,
        std::env::var("REBON_UI_MODE").ok().as_deref(),
        scan.user_ui.as_ref(),
        scan.project_ui.as_ref(),
        scan.flag_ui.as_ref(),
    )
}

pub(crate) fn validate_inline_settings(settings: &InlineUiSettings) -> anyhow::Result<()> {
    if !(4..=30).contains(&settings.viewport_height) {
        anyhow::bail!(
            "inline.viewportHeight must be between 4 and 30 (got {})",
            settings.viewport_height
        );
    }
    Ok(())
}

pub(crate) fn validate_status_line_config(config: &StatusLineConfig) -> anyhow::Result<()> {
    match config.kind {
        StatusLineKind::Command if config.command.trim().is_empty() => {
            anyhow::bail!("statusLine.command must be a non-empty string")
        }
        StatusLineKind::Command if config.script.is_some() => {
            anyhow::bail!("statusLine.command entries must not set script")
        }
        StatusLineKind::Script
            if match config.script.as_deref() {
                Some(path) => path.as_os_str().is_empty(),
                None => true,
            } =>
        {
            anyhow::bail!("statusLine.script must be a non-empty path")
        }
        StatusLineKind::Script if !config.command.is_empty() => {
            anyhow::bail!("statusLine.script entries must not set command")
        }
        StatusLineKind::Command | StatusLineKind::Script => {}
    }
    if let Some(padding) = config.padding {
        if padding > STATUS_LINE_MAX_PADDING {
            anyhow::bail!(
                "statusLine.padding must be between 0 and {} (got {})",
                STATUS_LINE_MAX_PADDING,
                padding
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(mode: Option<UiMode>, height: Option<u16>) -> UiSettingsSource {
        UiSettingsSource {
            ui_mode: mode,
            math_rendering: None,
            inline: height.map(|viewport_height| InlineUiSettings {
                viewport_height,
                ..InlineUiSettings::default()
            }),
            status_line: None,
        }
    }

    #[test]
    fn defaults_to_screen_with_math_rendering_off() {
        let resolved = resolve_ui_config(None, None, None, None, None).unwrap();
        assert_eq!(resolved.mode, UiMode::Screen);
        assert_eq!(resolved.math_rendering, MathRenderingMode::Off);
    }

    #[test]
    fn math_rendering_accepts_all_modes() {
        for mode in [
            MathRenderingMode::Off,
            MathRenderingMode::Unicode,
            MathRenderingMode::GraphicsAuto,
        ] {
            let user = UiSettingsSource {
                math_rendering: Some(mode),
                ..UiSettingsSource::default()
            };
            assert_eq!(
                resolve_ui_config(None, None, Some(&user), None, None)
                    .unwrap()
                    .math_rendering,
                mode
            );
        }
    }

    #[test]
    fn math_rendering_resolution_uses_flag_project_user_default_precedence() {
        let user = UiSettingsSource {
            math_rendering: Some(MathRenderingMode::Unicode),
            ..UiSettingsSource::default()
        };
        let project = UiSettingsSource {
            math_rendering: Some(MathRenderingMode::GraphicsAuto),
            ..UiSettingsSource::default()
        };
        let flag = UiSettingsSource {
            math_rendering: Some(MathRenderingMode::Off),
            ..UiSettingsSource::default()
        };

        assert_eq!(
            resolve_ui_config(None, None, Some(&user), Some(&project), None)
                .unwrap()
                .math_rendering,
            MathRenderingMode::GraphicsAuto
        );
        assert_eq!(
            resolve_ui_config(None, None, Some(&user), Some(&project), Some(&flag))
                .unwrap()
                .math_rendering,
            MathRenderingMode::Off
        );
    }

    #[test]
    fn resolution_precedence_is_cli_env_project_user_default() {
        let user = src(Some(UiMode::Inline), Some(10));
        let project = src(Some(UiMode::Screen), Some(11));
        assert_eq!(
            resolve_ui_config(None, None, Some(&user), Some(&project), None)
                .unwrap()
                .mode,
            UiMode::Screen
        );
        assert_eq!(
            resolve_ui_config(None, Some("inline"), Some(&user), Some(&project), None)
                .unwrap()
                .mode,
            UiMode::Inline
        );
        assert_eq!(
            resolve_ui_config(
                Some(UiMode::Screen),
                Some("inline"),
                Some(&user),
                Some(&project),
                None,
            )
            .unwrap()
            .mode,
            UiMode::Screen
        );
    }

    #[test]
    fn invalid_values_and_viewport_are_rejected() {
        assert!(resolve_ui_config(None, Some("float"), None, None, None).is_err());
        let bad = src(Some(UiMode::Inline), Some(31));
        assert!(resolve_ui_config(None, None, Some(&bad), None, None).is_err());
    }

    #[test]
    fn project_inline_settings_override_user_inline_settings() {
        let user = src(None, Some(9));
        let project = src(None, Some(13));
        assert_eq!(
            resolve_ui_config(None, None, Some(&user), Some(&project), None)
                .unwrap()
                .inline
                .viewport_height,
            13
        );
    }

    #[test]
    fn status_line_resolution_uses_flag_project_user_precedence() {
        let make = |command: &str| UiSettingsSource {
            status_line: Some(StatusLineConfig {
                kind: StatusLineKind::Command,
                command: command.to_string(),
                script: None,
                padding: None,
                refresh_interval: Some(1),
                hide_vim_mode_indicator: Some(false),
            }),
            ..UiSettingsSource::default()
        };
        let user = make("user");
        let project = make("project");
        let flag = make("flag");
        assert_eq!(
            resolve_ui_config(None, None, Some(&user), Some(&project), Some(&flag))
                .unwrap()
                .status_line
                .unwrap()
                .command,
            "flag"
        );
        assert_eq!(
            resolve_ui_config(None, None, Some(&user), Some(&project), None)
                .unwrap()
                .status_line
                .unwrap()
                .command,
            "project"
        );
    }
}
