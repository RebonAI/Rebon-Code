#[cfg(test)]
use std::path::Path;

use rebon_permissions::PermissionMode;

use super::BackgroundRuntimeFields;

pub(crate) fn permission_mode_from_wire_opt(
    raw: Option<&str>,
) -> anyhow::Result<Option<PermissionMode>> {
    Ok(match raw {
        Some("acceptEdits") => Some(PermissionMode::AcceptEdits),
        Some("bypassPermissions") => Some(PermissionMode::BypassPermissions),
        Some("default") => Some(PermissionMode::Default),
        Some("dontAsk") => Some(PermissionMode::DontAsk),
        Some("plan") => Some(PermissionMode::Plan),
        Some("auto") => Some(PermissionMode::Auto),
        Some(other) => anyhow::bail!("unknown background job permission mode `{other}`"),
        None => None,
    })
}

pub fn ensure_background_runtime_permission_mode_allowed(
    runtime: &BackgroundRuntimeFields,
) -> anyhow::Result<()> {
    if let Some(mode) = permission_mode_from_wire_opt(runtime.permission_mode.as_deref())? {
        crate::rebon_config::ensure_background_permission_mode_allowed(mode)?;
    }
    Ok(())
}

#[cfg(test)]
fn ensure_background_runtime_permission_mode_allowed_in(
    runtime: &BackgroundRuntimeFields,
    config_dir: &Path,
) -> anyhow::Result<()> {
    if let Some(mode) = permission_mode_from_wire_opt(runtime.permission_mode.as_deref())? {
        crate::rebon_config::ensure_background_permission_mode_allowed_in(config_dir, mode)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> BackgroundRuntimeFields {
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

    #[test]
    fn background_runtime_permission_gate_rejects_unaccepted_auto_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = runtime();
        runtime.permission_mode = Some("auto".into());

        let err =
            ensure_background_runtime_permission_mode_allowed_in(&runtime, tmp.path()).unwrap_err();

        assert!(err.to_string().contains("permission mode `auto`"));
        assert!(err.to_string().contains("interactive session"));
    }

    #[test]
    fn background_runtime_permission_gate_allows_accepted_bypass_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = runtime();
        runtime.permission_mode = Some("bypassPermissions".into());
        crate::rebon_config::mark_background_permission_mode_accepted_in(
            tmp.path(),
            PermissionMode::BypassPermissions,
        )
        .unwrap();

        ensure_background_runtime_permission_mode_allowed_in(&runtime, tmp.path()).unwrap();
    }

    /// `dontAsk` used to have no branch here, so a background job launched in
    /// that mode died at startup with "unknown background job permission
    /// mode" — the desktop settings page offers it as a persistable default,
    /// so the app could put a job into exactly that state.
    #[test]
    fn background_runtime_permission_mode_parses_dont_ask() {
        assert_eq!(
            permission_mode_from_wire_opt(Some("dontAsk")).unwrap(),
            Some(PermissionMode::DontAsk)
        );
    }

    /// It is fail-closed, so it needs no interactive acceptance the way auto
    /// and bypass do — it can only refuse more than the default, never less.
    #[test]
    fn background_runtime_permission_gate_allows_dont_ask_without_acceptance() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = runtime();
        runtime.permission_mode = Some("dontAsk".into());

        ensure_background_runtime_permission_mode_allowed_in(&runtime, tmp.path()).unwrap();
    }

    #[test]
    fn background_runtime_permission_gate_allows_safe_modes_without_acceptance() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = runtime();
        runtime.permission_mode = Some("acceptEdits".into());

        ensure_background_runtime_permission_mode_allowed_in(&runtime, tmp.path()).unwrap();
    }
}
