//! Foundation types shared across the agents module.
//!
//! Value types only — there is no runtime payload. [`AgentSummary`]
//! is the agent shape the reducers here operate on, with the system
//! prompt already resolved to a string.

use std::collections::BTreeMap;

/// All settings sources where an agent can come from.
///
/// Ordering matters — later sources override earlier sources at
/// lookup time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingSource {
    /// Global per-user settings (`~/.rebon/settings.json`).
    UserSettings,
    /// Shared per-directory settings checked into the repo.
    ProjectSettings,
    /// Per-directory settings that are gitignored.
    LocalSettings,
    /// Settings supplied via the `--settings` CLI flag.
    FlagSettings,
    /// Enterprise-managed policy settings.
    PolicySettings,
}

/// Source of an agent definition. Wider than [`SettingSource`] —
/// includes `built-in` and `plugin` agents that are not file-backed in
/// the same way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentSource {
    /// Compiled-in agents (general-purpose, plan, explore, etc.).
    BuiltIn,
    /// Agent provided by a loaded plugin. Carries the plugin name.
    Plugin {
        /// Plugin name (used for the `Plugin: <name>` display path).
        plugin: String,
    },
    /// Agent loaded from a settings source (.md file on disk).
    Settings(SettingSource),
}

impl AgentSource {
    /// Helper for comparing agent sources where two definitions are
    /// "the same source" iff they came from the same settings file.
    pub fn matches(&self, other: &AgentSource) -> bool {
        self == other
    }
}

/// Persistent agent memory scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentMemoryScope {
    /// Memory persists per-user across all projects.
    User,
    /// Memory persists per-project (gitignored local copy).
    Project,
}

impl AgentMemoryScope {
    /// Lowercase string used in YAML frontmatter (`memory: user`).
    pub fn as_str(self) -> &'static str {
        match self {
            AgentMemoryScope::User => "user",
            AgentMemoryScope::Project => "project",
        }
    }
}

/// Reasoning-effort hint for agents that use it.
///
/// Kept as a string because that's how the YAML frontmatter writes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffortValue(pub String);

impl EffortValue {
    /// Wraps a string into an [`EffortValue`].
    pub fn new(value: impl Into<String>) -> Self {
        EffortValue(value.into())
    }

    /// Borrows the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which runtime executes an agent, as far as the agents surface is
/// concerned.
///
/// A deliberate mirror of the engine's `runtime:` frontmatter — this
/// crate is dependency-free on purpose, so it carries the display
/// shape rather than importing the executable one, exactly as
/// [`AgentMemoryScope`] and [`EffortValue`] do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentRuntimeLabel {
    /// Rebon's own engine runs the turn.
    #[default]
    Local,
    /// A third-party CLI runs the turn, driven over ACP.
    Acp {
        /// The command line, when the definition named one.
        command: Option<String>,
    },
}

impl AgentRuntimeLabel {
    /// Short tag for the agent list (`local`, `acp`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Acp { .. } => "acp",
        }
    }

    /// Whether the agent runs outside Rebon's process.
    pub fn is_external(&self) -> bool {
        matches!(self, Self::Acp { .. })
    }

    /// One line for the detail view, naming the command when known.
    pub fn detail_line(&self) -> String {
        match self {
            Self::Local => "Local (rebon engine)".to_string(),
            Self::Acp {
                command: Some(command),
            } => format!("ACP: {command}"),
            Self::Acp { command: None } => "ACP (no command configured)".to_string(),
        }
    }

    /// What the host cannot promise for this agent, if anything.
    ///
    /// An external agent brings its own model and tools, and Rebon
    /// only sees the file writes it chooses to route back — so
    /// `/rewind` covers nothing it wrote directly, and its token usage
    /// is not reported. Saying so in the agent list beats letting the
    /// user find out when a rewind restores nothing.
    pub fn caveat(&self) -> Option<&'static str> {
        match self {
            Self::Local => None,
            Self::Acp { .. } => Some(
                "Runs outside rebon: brings its own model and tools. Token usage is not \
                 reported, and /rewind only covers writes it routes back through rebon.",
            ),
        }
    }
}

/// Light-weight projection of an agent definition carrying ONLY the
/// fields this module needs, for built-in, file-backed, and plugin
/// agents alike. The system prompt is stored already resolved, as a
/// plain `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    /// Stable identifier (kebab-case).
    pub agent_type: String,
    /// Description shown in pickers + saved as YAML `description`.
    pub when_to_use: String,
    /// Allowed tool names. `None` = all tools (omitted from YAML).
    pub tools: Option<Vec<String>>,
    /// Resolved system prompt body.
    pub system_prompt: String,
    /// Optional color label.
    pub color: Option<String>,
    /// Optional model identifier.
    pub model: Option<String>,
    /// Optional reasoning-effort hint.
    pub effort: Option<EffortValue>,
    /// Optional persistent memory scope.
    pub memory: Option<AgentMemoryScope>,
    /// Which runtime executes this agent.
    pub runtime: AgentRuntimeLabel,
    /// Where this agent came from.
    pub source: AgentSource,
    /// Original on-disk filename (without `.md`). `None` if it matches
    /// `agent_type`.
    pub filename: Option<String>,
}

impl AgentSummary {
    /// Convenience constructor for tests.
    pub fn minimal(
        agent_type: impl Into<String>,
        when_to_use: impl Into<String>,
        system_prompt: impl Into<String>,
        source: AgentSource,
    ) -> Self {
        AgentSummary {
            agent_type: agent_type.into(),
            when_to_use: when_to_use.into(),
            tools: None,
            system_prompt: system_prompt.into(),
            color: None,
            model: None,
            effort: None,
            memory: None,
            runtime: AgentRuntimeLabel::Local,
            source,
            filename: None,
        }
    }

    /// True if this is a built-in agent.
    pub fn is_built_in(&self) -> bool {
        matches!(self.source, AgentSource::BuiltIn)
    }

    /// True if this is a plugin agent.
    pub fn is_plugin(&self) -> bool {
        matches!(self.source, AgentSource::Plugin { .. })
    }

    /// True if this agent is editable (writable file-backed settings only).
    pub fn is_editable(&self) -> bool {
        matches!(
            self.source,
            AgentSource::Settings(
                SettingSource::UserSettings
                    | SettingSource::ProjectSettings
                    | SettingSource::LocalSettings
            )
        )
    }
}

/// Result of validating an agent definition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentValidationResult {
    /// True iff `errors` is empty; set by [`AgentValidationResult::new`].
    pub is_valid: bool,
    /// Hard errors (block save).
    pub errors: Vec<String>,
    /// Soft warnings (display only, do not block save).
    pub warnings: Vec<String>,
}

impl AgentValidationResult {
    /// Build a result from raw error / warning lists. Sets `is_valid`
    /// to `errors.is_empty()`.
    pub fn new(errors: Vec<String>, warnings: Vec<String>) -> Self {
        AgentValidationResult {
            is_valid: errors.is_empty(),
            errors,
            warnings,
        }
    }
}

/// Per-source agent groups, used by the list / menu views.
pub type AgentsBySource = BTreeMap<String, Vec<AgentSummary>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_summary_minimal_defaults() {
        let a = AgentSummary::minimal(
            "code-reviewer",
            "Use this agent when reviewing code",
            "You are a code reviewer.",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        assert_eq!(a.agent_type, "code-reviewer");
        assert_eq!(a.tools, None);
        assert!(a.color.is_none());
        assert!(a.is_editable());
        assert!(!a.is_built_in());
        assert!(!a.is_plugin());
    }

    #[test]
    fn built_in_agent_is_not_editable() {
        let a = AgentSummary::minimal(
            "general-purpose",
            "general purpose agent",
            "You are general purpose.",
            AgentSource::BuiltIn,
        );
        assert!(a.is_built_in());
        assert!(!a.is_editable());
    }

    #[test]
    fn plugin_agent_is_not_editable() {
        let a = AgentSummary::minimal(
            "plugin-agent",
            "plugin",
            "plugin prompt",
            AgentSource::Plugin {
                plugin: "my-plugin".into(),
            },
        );
        assert!(a.is_plugin());
        assert!(!a.is_editable());
    }

    #[test]
    fn flag_settings_agent_is_not_editable() {
        let a = AgentSummary::minimal(
            "flag-agent",
            "flag supplied",
            "flag prompt",
            AgentSource::Settings(SettingSource::FlagSettings),
        );
        assert!(!a.is_editable());
    }

    #[test]
    fn validation_result_is_valid_iff_errors_empty() {
        let ok = AgentValidationResult::new(vec![], vec!["warn".into()]);
        assert!(ok.is_valid);
        let not_ok = AgentValidationResult::new(vec!["err".into()], vec![]);
        assert!(!not_ok.is_valid);
    }

    #[test]
    fn agent_memory_scope_strings() {
        assert_eq!(AgentMemoryScope::User.as_str(), "user");
        assert_eq!(AgentMemoryScope::Project.as_str(), "project");
    }

    #[test]
    fn a_local_agent_promises_everything_and_says_nothing_extra() {
        let runtime = AgentRuntimeLabel::Local;
        assert_eq!(runtime, AgentRuntimeLabel::default());
        assert!(!runtime.is_external());
        assert_eq!(runtime.as_str(), "local");
        assert_eq!(runtime.detail_line(), "Local (rebon engine)");
        assert!(runtime.caveat().is_none());
    }

    #[test]
    fn an_external_agent_names_its_command_and_its_limits() {
        let runtime = AgentRuntimeLabel::Acp {
            command: Some("gemini --experimental-acp".into()),
        };
        assert!(runtime.is_external());
        assert_eq!(runtime.as_str(), "acp");
        assert_eq!(runtime.detail_line(), "ACP: gemini --experimental-acp");
        let caveat = runtime.caveat().expect("must warn about what it cannot do");
        assert!(caveat.contains("/rewind"));
        assert!(caveat.contains("usage"));
    }

    #[test]
    fn an_external_agent_without_a_command_still_reads_sensibly() {
        let runtime = AgentRuntimeLabel::Acp { command: None };
        assert_eq!(runtime.detail_line(), "ACP (no command configured)");
        assert!(runtime.caveat().is_some());
    }

    #[test]
    fn effort_value_round_trip() {
        let e = EffortValue::new("high");
        assert_eq!(e.as_str(), "high");
    }

    #[test]
    fn agent_source_matches() {
        let a = AgentSource::Settings(SettingSource::ProjectSettings);
        let b = AgentSource::Settings(SettingSource::ProjectSettings);
        let c = AgentSource::Settings(SettingSource::UserSettings);
        assert!(a.matches(&b));
        assert!(!a.matches(&c));
    }
}
