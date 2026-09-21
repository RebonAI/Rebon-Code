//! `AgentDetail` projector.
//!
//! Backs the read-only detail view with these rows (in order):
//!
//! 1. File path (relative).
//! 2. Description (`when_to_use`).
//! 3. Tools (resolved upstream by the consumer).
//! 4. Model (display string).
//! 5. Color.
//! 6. Memory scope.
//! 7. Runtime.
//! 8. Runtime caveat (omitted when the runtime promises everything).
//! 9. System prompt (rendered through Markdown).
//!
//! This module projects an `AgentSummary` to a [`AgentDetail`]
//! struct the consumer renders.

use crate::surface::types::{AgentMemoryScope, AgentSource, AgentSummary};
use crate::surface::validate::ResolvedTools;

/// Pre-built display rows for the AgentDetail surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDetail {
    /// Relative file path label (or special string for built-in /
    /// plugin / cli).
    pub file_path: String,
    /// `when_to_use` body, dedented for display.
    pub description: String,
    /// Tool list display rows.
    pub tools: ToolsDisplay,
    /// Model display string (`None` if not set).
    pub model: Option<String>,
    /// Color name (`None` if automatic).
    pub color: Option<String>,
    /// Memory scope display string (`None` if not set).
    pub memory: Option<String>,
    /// Which runtime executes the agent, e.g. `"Local (rebon engine)"`.
    pub runtime: String,
    /// What this runtime cannot promise, when that is worth saying.
    /// `None` for the local engine, which promises everything.
    pub runtime_caveat: Option<String>,
    /// System prompt body (raw markdown — the consumer renders it).
    pub system_prompt: String,
}

/// Tools display section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolsDisplay {
    /// "All tools" — agent declared `tools: undefined` or `["*"]`.
    AllTools,
    /// Empty list — agent has no tools.
    None,
    /// Explicit tool list, optionally with an "unrecognized" warning.
    Explicit {
        /// Valid tool names (rendered joined by ", ").
        valid: Vec<String>,
        /// Invalid tool names (also joined by ", ").
        invalid: Vec<String>,
    },
}

impl ToolsDisplay {
    /// True if the user should see a warning row.
    pub fn has_warning(&self) -> bool {
        matches!(self, ToolsDisplay::Explicit { invalid, .. } if !invalid.is_empty())
    }
}

/// Build the [`AgentDetail`] projection.
///
/// `file_path` is the result of [`crate::surface::agent_file::actual_relative_agent_file_path`]
/// (the consumer computes it upstream so this projector stays pure).
///
/// `resolved` is the consumer's upstream tool resolution.
pub fn build_agent_detail(
    agent: &AgentSummary,
    file_path: String,
    resolved: &ResolvedTools,
) -> AgentDetail {
    let tools = build_tools_display(agent, resolved);
    AgentDetail {
        file_path,
        description: agent.when_to_use.clone(),
        tools,
        model: agent.model.clone(),
        color: agent.color.clone(),
        memory: agent.memory.map(|m| memory_display_label(m).to_string()),
        runtime: agent.runtime.detail_line(),
        runtime_caveat: agent.runtime.caveat().map(str::to_string),
        system_prompt: agent.system_prompt.clone(),
    }
}

fn build_tools_display(agent: &AgentSummary, resolved: &ResolvedTools) -> ToolsDisplay {
    // Wildcard is modeled as "tools is None or tools == ["*"]" to
    // match `crate::surface::agent_file::format_agent_as_markdown`'s rule.
    let has_wildcard = match &agent.tools {
        None => true,
        Some(t) => t.len() == 1 && t[0] == "*",
    };
    if has_wildcard {
        return ToolsDisplay::AllTools;
    }
    let tools = agent.tools.clone().unwrap_or_default();
    if tools.is_empty() {
        return ToolsDisplay::None;
    }
    // Filter out anything in `invalid_tools` to compute the valid
    // list.
    let invalid: Vec<String> = resolved.invalid_tools.clone();
    let valid: Vec<String> = tools
        .iter()
        .filter(|t| !invalid.iter().any(|i| i == *t))
        .cloned()
        .collect();
    ToolsDisplay::Explicit { valid, invalid }
}

/// Display label for a memory scope.
pub fn memory_display_label(scope: AgentMemoryScope) -> &'static str {
    match scope {
        AgentMemoryScope::User => "User scope (~/.rebon/agent-memory/)",
        AgentMemoryScope::Project => "Project scope (.rebon/agent-memory/)",
    }
}

/// Pre-built `Source` display label.
pub fn source_label(source: &AgentSource) -> String {
    match source {
        AgentSource::BuiltIn => "Built-in".to_string(),
        AgentSource::Plugin { plugin } => format!("Plugin: {}", plugin),
        AgentSource::Settings(s) => match s {
            crate::surface::types::SettingSource::UserSettings => "User".into(),
            crate::surface::types::SettingSource::ProjectSettings => "Project".into(),
            crate::surface::types::SettingSource::LocalSettings => "Project, gitignored".into(),
            crate::surface::types::SettingSource::FlagSettings => "CLI flag".into(),
            crate::surface::types::SettingSource::PolicySettings => "Managed".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::types::SettingSource;

    fn agent() -> AgentSummary {
        let mut a = AgentSummary::minimal(
            "code-reviewer",
            "Use this agent when reviewing code",
            "You are a code reviewer.",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        a.color = Some("blue".into());
        a.model = Some("opus".into());
        a.memory = Some(AgentMemoryScope::User);
        a
    }

    #[test]
    fn detail_basic_fields() {
        let d = build_agent_detail(&agent(), "/path".into(), &ResolvedTools::default());
        assert_eq!(d.file_path, "/path");
        assert_eq!(d.description, "Use this agent when reviewing code");
        assert_eq!(d.model, Some("opus".into()));
        assert_eq!(d.color, Some("blue".into()));
        assert!(d.memory.unwrap().contains("User scope"));
    }

    #[test]
    fn detail_reports_the_runtime_and_stays_quiet_for_local_agents() {
        let d = build_agent_detail(&agent(), "/path".into(), &ResolvedTools::default());
        assert_eq!(d.runtime, "Local (rebon engine)");
        assert!(d.runtime_caveat.is_none());
    }

    #[test]
    fn detail_carries_the_external_runtime_caveat() {
        let mut a = agent();
        a.runtime = crate::surface::types::AgentRuntimeLabel::Acp {
            command: Some("gemini --experimental-acp".into()),
        };
        let d = build_agent_detail(&a, "/path".into(), &ResolvedTools::default());
        assert_eq!(d.runtime, "ACP: gemini --experimental-acp");
        assert!(d
            .runtime_caveat
            .expect("an external agent must say what it cannot do")
            .contains("/rewind"));
    }

    #[test]
    fn detail_no_tools_means_all_tools() {
        let d = build_agent_detail(&agent(), "x".into(), &ResolvedTools::default());
        assert_eq!(d.tools, ToolsDisplay::AllTools);
    }

    #[test]
    fn detail_star_tools_means_all_tools() {
        let mut a = agent();
        a.tools = Some(vec!["*".into()]);
        let d = build_agent_detail(&a, "x".into(), &ResolvedTools::default());
        assert_eq!(d.tools, ToolsDisplay::AllTools);
    }

    #[test]
    fn detail_empty_tools_means_none() {
        let mut a = agent();
        a.tools = Some(vec![]);
        let d = build_agent_detail(&a, "x".into(), &ResolvedTools::default());
        assert_eq!(d.tools, ToolsDisplay::None);
    }

    #[test]
    fn detail_explicit_tools_with_invalids() {
        let mut a = agent();
        a.tools = Some(vec!["FileReadTool".into(), "MissingTool".into()]);
        let resolved = ResolvedTools {
            invalid_tools: vec!["MissingTool".into()],
        };
        let d = build_agent_detail(&a, "x".into(), &resolved);
        match d.tools {
            ToolsDisplay::Explicit { valid, invalid } => {
                assert_eq!(valid, vec!["FileReadTool".to_string()]);
                assert_eq!(invalid, vec!["MissingTool".to_string()]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tools_display_warning_check() {
        let d = ToolsDisplay::Explicit {
            valid: vec![],
            invalid: vec!["x".into()],
        };
        assert!(d.has_warning());
        let d2 = ToolsDisplay::Explicit {
            valid: vec!["x".into()],
            invalid: vec![],
        };
        assert!(!d2.has_warning());
    }

    #[test]
    fn memory_labels() {
        assert!(memory_display_label(AgentMemoryScope::User).contains("User scope"));
        assert!(memory_display_label(AgentMemoryScope::Project).contains("Project scope"));
    }

    #[test]
    fn source_labels() {
        assert_eq!(
            source_label(&AgentSource::Settings(SettingSource::UserSettings)),
            "User"
        );
        assert_eq!(source_label(&AgentSource::BuiltIn), "Built-in");
        assert_eq!(
            source_label(&AgentSource::Plugin { plugin: "p".into() }),
            "Plugin: p"
        );
    }

    #[test]
    fn detail_carries_system_prompt() {
        let d = build_agent_detail(&agent(), "x".into(), &ResolvedTools::default());
        assert_eq!(d.system_prompt, "You are a code reviewer.");
    }
}
