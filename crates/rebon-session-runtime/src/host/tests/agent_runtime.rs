use super::super::*;

fn agent_def(
    agent_type: &str,
    runtime: rebon_tool::agent_registry::AgentRuntime,
) -> rebon_tool::agent_registry::ResolvedAgentDef {
    rebon_tool::agent_registry::ResolvedAgentDef {
        agent_type: agent_type.to_string(),
        when_to_use: "when testing".to_string(),
        system_prompt: "you are a test agent".to_string(),
        tool_filter: Default::default(),
        model: None,
        model_profile: None,
        provider: None,
        effort: None,
        background: false,
        isolation: None,
        memory: None,
        permission_mode: None,
        runtime,
        source: rebon_tool::agent_registry::AgentSource::BuiltIn,
        file_stem: None,
    }
}

#[test]
fn a_job_on_a_local_agent_definition_carries_its_prompt_and_tools() {
    let def = agent_def("reviewer", rebon_tool::agent_registry::AgentRuntime::Local);
    let resolved = background_agent_runtime_for(
        "review this".into(),
        Some(&def),
        Path::new("."),
        Some("/tmp/rebon/proj/job-1/scratchpad"),
    );

    assert_eq!(resolved.prompt, "review this");
    assert!(resolved
        .system
        .as_deref()
        .is_some_and(|system| system.contains("you are a test agent")));
    assert!(resolved
        .system
        .as_deref()
        .is_some_and(|system| system.contains("# Scratchpad Directory")
            && system.contains("/tmp/rebon/proj/job-1/scratchpad")));
    assert!(resolved.tool_filter.is_some());
    assert!(resolved.external_agent.is_none());
}

#[test]
fn a_job_on_an_external_agent_supplies_no_prompt_or_tools_of_its_own() {
    // The agent brings its own model, tools, and system prompt.
    // Handing it Rebon's would be describing a different agent —
    // and handing Rebon's engine an *empty* one, which is what used
    // to happen, silently runs a general-purpose local worker under
    // the external agent's name.
    let def = agent_def(
        "gemini",
        rebon_tool::agent_registry::AgentRuntime::Acp {
            command: "gemini".into(),
            args: vec!["--experimental-acp".into()],
        },
    );
    let resolved = background_agent_runtime_for("fix it".into(), Some(&def), Path::new("."), None);

    assert_eq!(resolved.external_agent.as_deref(), Some("gemini"));
    assert!(resolved.system.is_none());
    assert!(resolved.tool_filter.is_none());
    assert_eq!(resolved.prompt, "fix it", "the prompt still goes through");
}

#[test]
fn a_job_with_no_agent_runs_on_the_sessions_own_backend() {
    let resolved = background_agent_runtime_for("just do it".into(), None, Path::new("."), None);
    assert!(resolved.system.is_none());
    assert!(resolved.tool_filter.is_none());
    assert!(resolved.external_agent.is_none());
}
