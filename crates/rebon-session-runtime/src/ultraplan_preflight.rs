use std::path::{Path, PathBuf};

use rebon_types::{
    CapabilityContext, CapabilityDiagnostic, CapabilityDiagnosticClass, NetworkCapability,
    UltraplanRunState,
};

use crate::EngineSession;

const ULTRAPLAN_TOOLS: &[&str] = &[
    "Agent",
    "AskUserQuestion",
    "ExitPlanMode",
    "PlanLedger",
    "Read",
    "Glob",
    "Grep",
    "ToolSearch",
    "StructuredOutput",
];

pub fn preflight_ultraplan_run(
    session: &EngineSession,
    state: &mut UltraplanRunState,
) -> Result<CapabilityContext, CapabilityDiagnostic> {
    let cwd = Path::new(&session.cwd);
    let canonical = canonical_readable_root(state, cwd)?;
    let mut allowed_roots = vec![canonical.to_string_lossy().to_string()];
    for configured_root in &session.startup.add_dirs {
        let configured_root = PathBuf::from(configured_root);
        let requested = if configured_root.is_absolute() {
            configured_root
        } else {
            canonical.join(configured_root)
        };
        let root = canonical_readable_root(state, &requested)?;
        let root = root.to_string_lossy().to_string();
        if !allowed_roots.iter().any(|existing| existing == &root) {
            allowed_roots.push(root);
        }
    }

    let session_filter = session.engine_half.session_filter_handle.current();
    let tool_ids = ULTRAPLAN_TOOLS
        .iter()
        .filter_map(|tool_name| {
            let tool = session.engine_half.engine.find_tool(tool_name)?;
            session_filter
                .allows(tool.id().as_str(), tool.aliases())
                .then(|| (*tool_name).to_string())
        })
        .collect::<Vec<_>>();
    for required in [
        "AskUserQuestion",
        "ExitPlanMode",
        "PlanLedger",
        "Read",
        "Glob",
        "Grep",
    ] {
        if !tool_ids.iter().any(|tool| tool == required) {
            return Err(diagnostic(
                state,
                CapabilityDiagnosticClass::ToolUnavailable,
                format!("required ultraplan tool `{required}` is unavailable"),
                None,
                Some(required),
                false,
            ));
        }
    }

    let sub_agent_available =
        rebon_tool::sub_agents_enabled() && tool_ids.iter().any(|tool| tool == "Agent");
    let workspace_head = rebon_tool::worktree::git_current_head(&canonical).ok();
    let workspace_dirty = rebon_tool::worktree::git_status_porcelain(&canonical)
        .ok()
        .map(|status| !status.trim().is_empty());
    let mut capability = CapabilityContext {
        run_id: state.run_id.clone(),
        ledger_revision: state.ledger_revision,
        requirements_hash: state.requirements_hash.clone(),
        session_id: session.session_id.clone(),
        cwd: canonical.to_string_lossy().to_string(),
        allowed_roots,
        read_allowed: true,
        write_allowed: false,
        shell_allowed: false,
        tool_ids,
        network: NetworkCapability::Denied,
        workspace_head,
        workspace_dirty,
        provider: Some(session.model.provider_name.clone()),
        model: Some(session.model.name.clone()),
        sub_agent_available,
        max_research_agents: state.budget.max_research_agents,
        research_agents_used: state.budget.research_agents_used,
        max_adversarial_reviews: state.budget.max_adversarial_reviews,
        adversarial_reviews_used: state.budget.adversarial_reviews_used,
        max_tool_error_retries: state.budget.max_tool_error_retries,
        capability_hash: String::new(),
    };
    capability.refresh_hash();
    state.set_capability_context(capability);
    Ok(state
        .capability_context
        .clone()
        .expect("set_capability_context must store the snapshot"))
}

pub(crate) fn preflight_ultraplan_worker(
    session: &EngineSession,
    state: &UltraplanRunState,
    role: &str,
    required_tools: &[&str],
) -> Result<CapabilityContext, CapabilityDiagnostic> {
    let capability = state.capability_context.clone().ok_or_else(|| {
        diagnostic(
            state,
            CapabilityDiagnosticClass::CapabilityDrift,
            "active ultraplan run has no CapabilityContext",
            None,
            Some("capability_context"),
            true,
        )
    })?;
    if !capability.hash_is_valid()
        || capability.run_id != state.run_id
        || capability.ledger_revision != state.ledger_revision
        || capability.requirements_hash != state.requirements_hash
    {
        return Err(diagnostic(
            state,
            CapabilityDiagnosticClass::CapabilityDrift,
            "active ultraplan CapabilityContext is stale",
            None,
            Some("capability_hash"),
            true,
        ));
    }
    if capability.session_id != session.session_id {
        return Err(diagnostic(
            state,
            CapabilityDiagnosticClass::SessionScopeMismatch,
            format!(
                "worker session `{}` does not match run session `{}`",
                session.session_id, capability.session_id
            ),
            None,
            Some("session_id"),
            true,
        ));
    }
    if !capability.sub_agent_available {
        return Err(diagnostic(
            state,
            CapabilityDiagnosticClass::ToolUnavailable,
            "sub-agent spawning is disabled; use parent-session serial research",
            None,
            Some("sub_agent"),
            false,
        ));
    }
    for tool in required_tools {
        if !capability
            .tool_ids
            .iter()
            .any(|available| available == tool)
            || session.engine_half.engine.find_tool(tool).is_none()
        {
            return Err(diagnostic(
                state,
                CapabilityDiagnosticClass::ToolUnavailable,
                format!("worker role `{role}` requires unavailable tool `{tool}`"),
                None,
                Some(tool),
                false,
            ));
        }
    }
    for root in &capability.allowed_roots {
        let root_path = Path::new(root);
        if !root_path.is_dir() {
            return Err(diagnostic(
                state,
                CapabilityDiagnosticClass::MissingRoot,
                format!("worker root `{root}` does not exist"),
                Some(root_path),
                Some("allowed_roots"),
                false,
            ));
        }
        std::fs::read_dir(root_path).map_err(|err| {
            diagnostic(
                state,
                CapabilityDiagnosticClass::ReadDenied,
                format!("worker cannot read root `{root}`: {err}"),
                Some(root_path),
                Some("read"),
                false,
            )
        })?;
    }
    Ok(capability)
}

fn canonical_readable_root(
    state: &UltraplanRunState,
    requested: &Path,
) -> Result<PathBuf, CapabilityDiagnostic> {
    let canonical = std::fs::canonicalize(requested).map_err(|_| {
        diagnostic(
            state,
            CapabilityDiagnosticClass::MissingRoot,
            format!("ultraplan root `{}` does not exist", requested.display()),
            Some(requested),
            Some("allowed_roots"),
            false,
        )
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|err| {
        diagnostic(
            state,
            CapabilityDiagnosticClass::ReadDenied,
            format!(
                "cannot inspect ultraplan root `{}`: {err}",
                canonical.display()
            ),
            Some(&canonical),
            Some("read"),
            false,
        )
    })?;
    if !metadata.is_dir() {
        return Err(diagnostic(
            state,
            CapabilityDiagnosticClass::MissingRoot,
            format!(
                "ultraplan root `{}` is not a directory",
                canonical.display()
            ),
            Some(&canonical),
            Some("allowed_roots"),
            false,
        ));
    }
    std::fs::read_dir(&canonical).map_err(|err| {
        diagnostic(
            state,
            CapabilityDiagnosticClass::ReadDenied,
            format!(
                "cannot read ultraplan root `{}`: {err}",
                canonical.display()
            ),
            Some(&canonical),
            Some("read"),
            false,
        )
    })?;
    Ok(canonical)
}

pub fn diagnostic(
    state: &UltraplanRunState,
    class: CapabilityDiagnosticClass,
    message: impl Into<String>,
    root: Option<&Path>,
    capability: Option<&str>,
    retryable: bool,
) -> CapabilityDiagnostic {
    CapabilityDiagnostic {
        class,
        message: message.into(),
        run_id: state.run_id.clone(),
        ledger_revision: state.ledger_revision,
        capability_hash: state
            .capability_context
            .as_ref()
            .map(|context| context.capability_hash.clone())
            .unwrap_or_default(),
        role: "ultraplan".into(),
        root: root.map(|path| path.to_string_lossy().to_string()),
        capability: capability.map(str::to_string),
        retryable,
        fallback_to_parent: true,
    }
}
