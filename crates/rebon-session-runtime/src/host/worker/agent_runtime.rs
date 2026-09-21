use super::super::*;

pub(crate) fn background_prompt_coordinator_report_paths(
    state: &BackgroundJobState,
) -> Vec<String> {
    // Job-level grants, not just the prompt being claimed: reports stay
    // readable across turns. The claimed prompt's own paths were folded
    // into the grants when it was enqueued, so this is a superset.
    let mut paths = state.identity.coordinator_report_grants.clone();
    for path in claimed_pending_prompt(state)
        .map(|prompt| prompt.coordinator_report_paths.as_slice())
        .unwrap_or_default()
    {
        if !paths.iter().any(|granted| granted == path) {
            paths.push(path.clone());
        }
    }
    paths
}

pub(crate) fn background_prompt_images_for_execution(
    state: &BackgroundJobState,
) -> &[BackgroundImageAttachment] {
    claimed_pending_prompt(state)
        .map(|prompt| prompt.images.as_slice())
        .unwrap_or(&state.identity.prompt_images)
}

/// What a job's `--agent` selection means for the turn about to run.
pub(crate) struct BackgroundAgentRuntime {
    pub(crate) prompt: String,
    /// System prompt for a local agent definition.
    pub(crate) system: Option<String>,
    /// Tool filter for a local agent definition.
    pub(crate) tool_filter: Option<rebon_types::ToolFilterSpec>,
    /// The agent CLI to run the job on, when the definition is
    /// external. Mutually exclusive with the two above: an external
    /// agent brings its own model, tools, and system prompt, so Rebon
    /// has none to supply.
    pub(crate) external_agent: Option<String>,
}

impl BackgroundAgentRuntime {
    fn local(prompt: String) -> Self {
        Self {
            prompt,
            system: None,
            tool_filter: None,
            external_agent: None,
        }
    }
}

pub(crate) fn resolve_background_agent_runtime(
    state: &BackgroundJobState,
) -> BackgroundAgentRuntime {
    let prompt = claimed_pending_prompt(state)
        .map(|prompt| prompt.text.clone())
        .unwrap_or_else(|| state.identity.prompt.clone());
    let Some(agent_type) = state.identity.agent_type.as_deref() else {
        return BackgroundAgentRuntime::local(prompt);
    };
    let cwd = PathBuf::from(&state.identity.cwd);
    let registry = rebon_tool::AgentRegistry::load(&cwd, &crate::rebon_config::config_home_dir());
    // Keyed by the job's session when it already has one so restarts
    // keep pointing the agent at the same scratchpad.
    let scratchpad_key = state
        .identity
        .session_id
        .clone()
        .unwrap_or_else(|| state.identity.job_id.clone());
    let scratchpad_dir =
        rebon_core::system_prompt::scratchpad_dir_for(&state.identity.cwd, &scratchpad_key);
    background_agent_runtime_for(
        prompt,
        registry.resolve(agent_type),
        &cwd,
        Some(&scratchpad_dir),
    )
}

/// The decision itself, once the definition has been resolved.
///
/// Split out from the registry load so the branch that matters — local
/// definition versus external agent — is reachable without a project
/// tree on disk.
pub(crate) fn background_agent_runtime_for(
    prompt: String,
    def: Option<&rebon_tool::agent_registry::ResolvedAgentDef>,
    cwd: &Path,
    scratchpad_dir: Option<&str>,
) -> BackgroundAgentRuntime {
    let Some(def) = def else {
        return BackgroundAgentRuntime::local(prompt);
    };
    if def.runtime.is_external() {
        // No system prompt, no tool filter: this job is going to run on
        // somebody else's agent, which supplies both itself.
        return BackgroundAgentRuntime {
            external_agent: Some(def.agent_type.clone()),
            ..BackgroundAgentRuntime::local(prompt)
        };
    }
    let mut system = def.system_prompt.clone();
    if let Some(scope_raw) = def.memory.as_deref() {
        if let Some(scope) =
            rebon_plugin_memory::memory::agent_memory::AgentMemoryScope::parse(scope_raw)
        {
            let memory_prompt = rebon_plugin_memory::memory::agent_memory::load_agent_memory_prompt(
                &def.agent_type,
                scope,
                cwd,
            );
            if system.trim().is_empty() {
                system = memory_prompt;
            } else {
                system = format!("{system}\n\n{memory_prompt}");
            }
        }
    }
    if !system.trim().is_empty() {
        // Same suffix the sub-agent spawner appends: shared notes plus
        // the scratchpad pointer so temp artifacts stay out of the
        // project tree.
        let suffix = match scratchpad_dir {
            Some(dir) => rebon_core::system_prompt::sub_agent_prompt_suffix(dir),
            None => rebon_core::system_prompt::sub_agent_notes_section().to_string(),
        };
        system = format!("{system}\n\n{suffix}");
    }
    BackgroundAgentRuntime {
        prompt,
        system: Some(system),
        tool_filter: Some(def.tool_filter.to_spec()),
        external_agent: None,
    }
}
