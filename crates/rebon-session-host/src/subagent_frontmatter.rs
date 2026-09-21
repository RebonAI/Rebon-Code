use std::path::Path;

use super::BackgroundRuntimeFields;

/// Fill in `runtime` fields with the resolved subagent's frontmatter,
/// without overwriting fields the caller already specified.
///
/// CLI / dispatch-time flags win — this helper only supplies defaults
/// (per Agent Views.md: "the `permissionMode` from the dispatched
/// subagent's frontmatter" applies when not explicitly overridden).
/// Returns a JSON object describing what was applied so the launch
/// path can record an `agent_runtime_applied` event for observability.
///
/// `config_home_dir` is the rebon config home (`store.root()`); the
/// agent registry is resolved relative to `cwd` + this directory.
pub fn apply_subagent_frontmatter_to_runtime(
    runtime: &mut BackgroundRuntimeFields,
    cwd: &Path,
    agent_type: &str,
    config_home_dir: &Path,
) -> Option<serde_json::Value> {
    apply_subagent_frontmatter_to_runtime_with_registry(
        runtime,
        agent_type,
        &rebon_tool::AgentRegistry::load(cwd, config_home_dir),
    )
}

fn apply_subagent_frontmatter_to_runtime_with_registry(
    runtime: &mut BackgroundRuntimeFields,
    agent_type: &str,
    registry: &rebon_tool::AgentRegistry,
) -> Option<serde_json::Value> {
    let def = registry.resolve(agent_type)?;
    let mut applied = serde_json::Map::new();
    if runtime.model.is_none() {
        if let Some(model) = def.model.clone() {
            applied.insert("model".into(), serde_json::Value::String(model.clone()));
            runtime.model = Some(model);
        }
    }
    if runtime.provider.is_none() {
        if let Some(provider) = def.provider.clone() {
            applied.insert(
                "provider".into(),
                serde_json::Value::String(provider.clone()),
            );
            runtime.provider = Some(provider);
        }
    }
    if runtime.effort_level.is_none() {
        if let Some(raw) = def.effort.as_deref() {
            if let Some(level) = normalize_agent_effort(raw) {
                applied.insert(
                    "effortLevel".into(),
                    serde_json::Value::String(level.to_string()),
                );
                runtime.effort_level = Some(level.to_string());
            }
        }
    }
    if runtime.permission_mode.is_none() {
        if let Some(mode) = def.permission_mode.clone() {
            applied.insert(
                "permissionMode".into(),
                serde_json::Value::String(mode.clone()),
            );
            runtime.permission_mode = Some(mode);
        }
    }
    if let Some(isolation) = def.isolation.as_deref() {
        // Record-only for now: `prepare_background_worktree` already
        // attempts a worktree by default in git repos, so the hint is
        // currently observability-only. A follow-up will force a
        // worktree for non-git cwds when isolation == "worktree".
        applied.insert(
            "isolation".into(),
            serde_json::Value::String(isolation.to_string()),
        );
    }
    if def.background {
        applied.insert("background".into(), serde_json::Value::Bool(true));
    }
    if applied.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(applied))
    }
}

fn normalize_agent_effort(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "low" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "x_high" | "extra_high" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
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

    fn registry_with_agent(def: rebon_tool::ResolvedAgentDef) -> rebon_tool::AgentRegistry {
        rebon_tool::AgentRegistry::from_groups(rebon_tool::AgentGroups {
            user: vec![def],
            ..Default::default()
        })
    }

    fn fixture_agent(name: &str) -> rebon_tool::ResolvedAgentDef {
        rebon_tool::ResolvedAgentDef {
            agent_type: name.to_string(),
            when_to_use: "fixture".to_string(),
            system_prompt: "system".to_string(),
            tool_filter: rebon_tool::ToolFilter::unrestricted(),
            model: None,
            model_profile: None,
            provider: None,
            effort: None,
            background: false,
            isolation: None,
            memory: None,
            permission_mode: None,
            runtime: rebon_tool::AgentRuntime::Local,
            source: rebon_tool::AgentSource::Settings(rebon_tool::SettingSource::UserSettings),
            file_stem: None,
        }
    }

    #[test]
    fn apply_subagent_frontmatter_fills_unset_runtime_fields() {
        let registry = registry_with_agent(rebon_tool::ResolvedAgentDef {
            model: Some("sonnet".to_string()),
            provider: Some("anthropic".to_string()),
            effort: Some("high".to_string()),
            isolation: Some("worktree".to_string()),
            background: true,
            ..fixture_agent("fixture-agent")
        });
        let mut runtime = runtime();

        let applied = apply_subagent_frontmatter_to_runtime_with_registry(
            &mut runtime,
            "fixture-agent",
            &registry,
        )
        .expect("agent applied");

        assert_eq!(runtime.model.as_deref(), Some("sonnet"));
        assert_eq!(runtime.provider.as_deref(), Some("anthropic"));
        assert_eq!(runtime.effort_level.as_deref(), Some("high"));
        assert_eq!(applied["model"], serde_json::json!("sonnet"));
        assert_eq!(applied["provider"], serde_json::json!("anthropic"));
        assert_eq!(applied["effortLevel"], serde_json::json!("high"));
        assert_eq!(applied["isolation"], serde_json::json!("worktree"));
        assert_eq!(applied["background"], serde_json::json!(true));
    }

    #[test]
    fn apply_subagent_frontmatter_does_not_override_caller_set_fields() {
        let registry = registry_with_agent(rebon_tool::ResolvedAgentDef {
            model: Some("sonnet".to_string()),
            provider: Some("anthropic".to_string()),
            effort: Some("high".to_string()),
            ..fixture_agent("fixture-agent")
        });
        let mut runtime = runtime();
        runtime.model = Some("explicit-model".into());
        runtime.provider = Some("explicit-provider".into());
        runtime.effort_level = Some("low".into());

        let applied = apply_subagent_frontmatter_to_runtime_with_registry(
            &mut runtime,
            "fixture-agent",
            &registry,
        );

        assert_eq!(runtime.model.as_deref(), Some("explicit-model"));
        assert_eq!(runtime.provider.as_deref(), Some("explicit-provider"));
        assert_eq!(runtime.effort_level.as_deref(), Some("low"));
        // Only the non-overridable hint (isolation/background) would be
        // reported here; this fixture has neither, so nothing applied.
        assert!(applied.is_none());
    }

    #[test]
    fn apply_subagent_frontmatter_normalizes_effort_aliases() {
        let registry = registry_with_agent(rebon_tool::ResolvedAgentDef {
            effort: Some("Extra-High".to_string()),
            ..fixture_agent("fixture-effort")
        });
        let mut runtime = runtime();

        let applied = apply_subagent_frontmatter_to_runtime_with_registry(
            &mut runtime,
            "fixture-effort",
            &registry,
        )
        .expect("agent applied");

        assert_eq!(runtime.effort_level.as_deref(), Some("xhigh"));
        assert_eq!(applied["effortLevel"], serde_json::json!("xhigh"));
    }

    #[test]
    fn apply_subagent_frontmatter_propagates_permission_mode() {
        let registry = registry_with_agent(rebon_tool::ResolvedAgentDef {
            permission_mode: Some("acceptEdits".to_string()),
            ..fixture_agent("fixture-permission")
        });
        let mut runtime = runtime();

        let applied = apply_subagent_frontmatter_to_runtime_with_registry(
            &mut runtime,
            "fixture-permission",
            &registry,
        )
        .expect("agent applied");

        assert_eq!(runtime.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(applied["permissionMode"], serde_json::json!("acceptEdits"));
    }

    #[test]
    fn apply_subagent_frontmatter_does_not_override_caller_permission_mode() {
        let registry = registry_with_agent(rebon_tool::ResolvedAgentDef {
            permission_mode: Some("acceptEdits".to_string()),
            ..fixture_agent("fixture-permission")
        });
        let mut runtime = runtime();
        runtime.permission_mode = Some("plan".into());

        let _ = apply_subagent_frontmatter_to_runtime_with_registry(
            &mut runtime,
            "fixture-permission",
            &registry,
        );

        assert_eq!(runtime.permission_mode.as_deref(), Some("plan"));
    }

    #[test]
    fn apply_subagent_frontmatter_returns_none_when_agent_not_found() {
        let registry = registry_with_agent(fixture_agent("known-agent"));
        let mut runtime = runtime();
        let applied = apply_subagent_frontmatter_to_runtime_with_registry(
            &mut runtime,
            "missing-agent",
            &registry,
        );
        assert!(applied.is_none());
        assert!(runtime.model.is_none());
    }
}
