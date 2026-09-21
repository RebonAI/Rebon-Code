use rebon_types::ReasoningEffort;

use rebon_session_host::BackgroundRuntimeFields;

use super::runtime_permissions::permission_mode_from_wire_opt;

/// rebon-cli-only conversions between the stored [`BackgroundRuntimeFields`]
/// (which lives in the framework-agnostic `rebon-session-host` crate) and the
/// binary's `RuntimeOverride` — the latter references
/// `crate::ui_config::UiMode`, so the conversion cannot live in the library.
///
/// Kept as an extension trait so every existing call site
/// (`BackgroundRuntimeFields::from_runtime_override(..)` / `.to_runtime_override()`)
/// compiles unchanged once `RuntimeFieldsExt` is in scope.
pub trait RuntimeFieldsExt: Sized {
    fn from_runtime_override(overrides: &crate::rebon_config::RuntimeOverride) -> Self;
    fn with_provider_format(self, format: crate::rebon_config::ProviderFormat) -> Self;
    fn to_runtime_override(&self) -> anyhow::Result<crate::rebon_config::RuntimeOverride>;
}

impl RuntimeFieldsExt for BackgroundRuntimeFields {
    fn from_runtime_override(overrides: &crate::rebon_config::RuntimeOverride) -> Self {
        Self {
            provider: overrides.provider.clone(),
            model: overrides.model.clone(),
            fast_mode: overrides.fast_mode,
            channels: overrides.channels.iter().map(ToString::to_string).collect(),
            development_channels: overrides
                .development_channels
                .iter()
                .map(ToString::to_string)
                .collect(),
            provider_format: None,
            ui_mode: overrides.ui_mode.map(|mode| mode.to_string()),
            effort_level: overrides
                .effort_level
                .map(|level| level.as_str().to_string()),
            permission_mode: overrides
                .permission_mode
                .map(|mode| mode.as_wire().to_string()),
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: overrides.settings.clone(),
            add_dirs: overrides.add_dirs.clone(),
            plugin_dirs: overrides.plugin_dirs.clone(),
            mcp_configs: overrides.mcp_configs.clone(),
            strict_mcp_config: overrides.strict_mcp_config,
        }
    }

    fn with_provider_format(mut self, format: crate::rebon_config::ProviderFormat) -> Self {
        self.provider_format = Some(provider_format_label(format).to_string());
        self
    }

    fn to_runtime_override(&self) -> anyhow::Result<crate::rebon_config::RuntimeOverride> {
        let channels = rebon_plugin_mcp::runtime::parse_channel_entries(&self.channels)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        let development_channels =
            rebon_plugin_mcp::runtime::parse_channel_entries(&self.development_channels)
                .map_err(|err| anyhow::anyhow!(err.to_string()))?
                .into_iter()
                .map(|entry| entry.with_dev(true))
                .collect();
        let ui_mode = match self.ui_mode.as_deref() {
            Some("screen") => Some(crate::ui_config::UiMode::Screen),
            Some("inline") => Some(crate::ui_config::UiMode::Inline),
            Some(other) => anyhow::bail!("unknown background job ui mode `{other}`"),
            None => None,
        };
        let effort_level = match self.effort_level.as_deref() {
            Some("low") => Some(ReasoningEffort::Low),
            Some("medium") => Some(ReasoningEffort::Medium),
            Some("high") => Some(ReasoningEffort::High),
            Some("xhigh") => Some(ReasoningEffort::XHigh),
            Some("max") => Some(ReasoningEffort::Max),
            Some(other) => anyhow::bail!("unknown background job effort level `{other}`"),
            None => None,
        };
        let permission_mode = permission_mode_from_wire_opt(self.permission_mode.as_deref())?;
        Ok(crate::rebon_config::RuntimeOverride {
            provider: self.provider.clone(),
            model: self.model.clone(),
            fast_mode: self.fast_mode,
            resume: None,
            cwd: None,
            channels,
            development_channels,
            ui_mode,
            effort_level,
            permission_mode,
            settings: self.settings.clone(),
            add_dirs: self.add_dirs.clone(),
            plugin_dirs: self.plugin_dirs.clone(),
            mcp_configs: self.mcp_configs.clone(),
            strict_mcp_config: self.strict_mcp_config,
            queue_session: false,
            startup_agent_view: false,
            startup_hosted: false,
            startup_local: false,
            startup_agent_view_cwd_scope: None,
            startup_notices: Vec::new(),
            attached_background_job_id: None,
            // Not carried in the job record, and it does not need to
            // be: a background job rebuilds the session it belongs to,
            // and a remote session's agent choice already lives in the
            // session sidecar, so `initial_agent` reopens it on the
            // same remote. Replaying `--remote` here would be a second
            // source of truth for the same fact.
            remote: None,
            remote_path: None,
        })
    }
}

pub(crate) fn effort_level_from_runtime(
    runtime: &BackgroundRuntimeFields,
) -> anyhow::Result<Option<ReasoningEffort>> {
    runtime
        .effort_level
        .as_deref()
        .map(effort_level_from_wire)
        .transpose()
}

/// Parse one effort level, so a caller changing it can refuse a bad value at
/// the point of the request instead of letting the next turn fail to start.
pub fn effort_level_from_wire(value: &str) -> anyhow::Result<ReasoningEffort> {
    Ok(match value {
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        "max" => ReasoningEffort::Max,
        other => anyhow::bail!("unknown background job effort level `{other}`"),
    })
}

fn provider_format_label(format: crate::rebon_config::ProviderFormat) -> &'static str {
    match format {
        crate::rebon_config::ProviderFormat::Openai => "openai",
        crate::rebon_config::ProviderFormat::OpenaiResponses => "openai-responses",
        crate::rebon_config::ProviderFormat::Anthropic => "anthropic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rebon_permissions::PermissionMode;

    #[test]
    fn runtime_fields_round_trip_effort_level() {
        let overrides = crate::rebon_config::RuntimeOverride {
            provider: Some("openai".into()),
            model: Some("gpt".into()),
            fast_mode: Some(false),
            resume: None,
            cwd: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            settings: vec!["settings.json".into()],
            add_dirs: vec!["../shared".into()],
            plugin_dirs: vec!["../plugin".into()],
            mcp_configs: vec!["{}".into()],
            strict_mcp_config: true,
            ui_mode: None,
            effort_level: Some(ReasoningEffort::XHigh),
            permission_mode: Some(PermissionMode::Auto),
            queue_session: false,
            startup_agent_view: false,
            startup_hosted: false,
            startup_local: false,
            startup_agent_view_cwd_scope: None,
            startup_notices: Vec::new(),
            attached_background_job_id: None,
            remote: None,
            remote_path: None,
        };

        let fields = BackgroundRuntimeFields::from_runtime_override(&overrides);
        assert_eq!(fields.effort_level.as_deref(), Some("xhigh"));
        assert_eq!(fields.fast_mode, Some(false));
        assert_eq!(fields.permission_mode.as_deref(), Some("auto"));
        assert_eq!(fields.settings, vec!["settings.json".to_string()]);
        assert_eq!(fields.add_dirs, vec!["../shared".to_string()]);
        assert_eq!(fields.plugin_dirs, vec!["../plugin".to_string()]);
        assert_eq!(fields.mcp_configs, vec!["{}".to_string()]);
        assert!(fields.strict_mcp_config);
        let runtime = fields.to_runtime_override().unwrap();
        assert_eq!(runtime.effort_level, Some(ReasoningEffort::XHigh));
        assert_eq!(runtime.fast_mode, Some(false));
        assert_eq!(runtime.permission_mode, Some(PermissionMode::Auto));
        assert_eq!(runtime.settings, vec!["settings.json".to_string()]);
        assert_eq!(runtime.add_dirs, vec!["../shared".to_string()]);
        assert_eq!(runtime.plugin_dirs, vec!["../plugin".to_string()]);
        assert_eq!(runtime.mcp_configs, vec!["{}".to_string()]);
        assert!(runtime.strict_mcp_config);
    }

    /// The desktop app changes a running session's mode by writing
    /// `runtime.permission_mode` into the job state; the worker rebuilds its
    /// session from these fields on the next turn. `bypassPermissions` has to
    /// survive that round trip intact.
    #[test]
    fn runtime_fields_round_trip_bypass_permissions() {
        let fields = BackgroundRuntimeFields {
            permission_mode: Some("bypassPermissions".into()),
            ..empty_runtime_fields()
        };

        let runtime = fields.to_runtime_override().unwrap();
        assert_eq!(
            runtime.permission_mode,
            Some(PermissionMode::BypassPermissions)
        );
        assert_eq!(
            BackgroundRuntimeFields::from_runtime_override(&runtime)
                .permission_mode
                .as_deref(),
            Some("bypassPermissions")
        );
    }

    #[test]
    fn runtime_fields_round_trip_dont_ask() {
        let fields = BackgroundRuntimeFields {
            permission_mode: Some("dontAsk".into()),
            ..empty_runtime_fields()
        };

        let runtime = fields.to_runtime_override().unwrap();
        assert_eq!(runtime.permission_mode, Some(PermissionMode::DontAsk));
        assert_eq!(
            BackgroundRuntimeFields::from_runtime_override(&runtime)
                .permission_mode
                .as_deref(),
            Some("dontAsk")
        );
    }

    fn empty_runtime_fields() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }
}
