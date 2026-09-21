//! Small pure helpers for the agents module.

use crate::surface::types::SettingSource;

/// One of the wider source variants accepted by the picker / list
/// surface — includes pseudo-sources `all`, `built-in`, and `plugin`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentSourceFilter {
    /// Pseudo-source matching every agent.
    All,
    /// Compiled-in agents only.
    BuiltIn,
    /// Plugin-supplied agents only.
    Plugin,
    /// Agents from a specific settings source.
    Source(SettingSource),
}

/// Display name shown above an agent group / picker. Settings sources
/// use their capitalized short name (`User`, `Project, gitignored`,
/// `Cli flag`, …).
pub fn agent_source_display_name(source: &AgentSourceFilter) -> &'static str {
    match source {
        AgentSourceFilter::All => "Agents",
        AgentSourceFilter::BuiltIn => "Built-in agents",
        AgentSourceFilter::Plugin => "Plugin agents",
        AgentSourceFilter::Source(s) => match s {
            SettingSource::UserSettings => "User",
            SettingSource::ProjectSettings => "Project",
            SettingSource::LocalSettings => "Project, gitignored",
            SettingSource::FlagSettings => "Cli flag",
            SettingSource::PolicySettings => "Managed",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_all() {
        assert_eq!(agent_source_display_name(&AgentSourceFilter::All), "Agents");
    }

    #[test]
    fn display_built_in() {
        assert_eq!(
            agent_source_display_name(&AgentSourceFilter::BuiltIn),
            "Built-in agents"
        );
    }

    #[test]
    fn display_plugin() {
        assert_eq!(
            agent_source_display_name(&AgentSourceFilter::Plugin),
            "Plugin agents"
        );
    }

    #[test]
    fn display_user_settings() {
        assert_eq!(
            agent_source_display_name(&AgentSourceFilter::Source(SettingSource::UserSettings)),
            "User"
        );
    }

    #[test]
    fn display_project_settings() {
        assert_eq!(
            agent_source_display_name(&AgentSourceFilter::Source(SettingSource::ProjectSettings)),
            "Project"
        );
    }

    #[test]
    fn display_local_settings() {
        assert_eq!(
            agent_source_display_name(&AgentSourceFilter::Source(SettingSource::LocalSettings)),
            "Project, gitignored"
        );
    }

    #[test]
    fn display_flag_settings() {
        assert_eq!(
            agent_source_display_name(&AgentSourceFilter::Source(SettingSource::FlagSettings)),
            "Cli flag"
        );
    }

    #[test]
    fn display_policy_settings() {
        assert_eq!(
            agent_source_display_name(&AgentSourceFilter::Source(SettingSource::PolicySettings)),
            "Managed"
        );
    }
}
