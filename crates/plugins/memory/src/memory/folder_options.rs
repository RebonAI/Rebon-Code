//! Builder for the auto-memory / team-memory / agent-memory
//! "Open … folder" rows.
//!
//! ## Behavior notes
//!
//! [`build_folder_options`] emits, in order:
//!
//! 1. An auto-memory row, when the master gate is on.
//! 2. A team-memory row, when a team-memory path is configured.
//! 3. One row per agent memory entry, in iteration order.
//!
//! Five load-bearing details:
//!
//! 1. **Folder rows are gated on `auto_memory_enabled`.** If
//!    auto-memory is off, no folder rows are produced — even if
//!    team-memory is enabled or agents have memory directories.
//! 2. **Auto-memory row is always first** when the gate fires.
//! 3. **Team-memory row needs a configured path.** The crate models
//!    the feature flag plus the runtime gate as "team_memory_path is
//!    `Some(_)`".
//! 4. **Agent rows iterate the `agents` slice in order**, skipping
//!    entries whose `memory` field is empty.
//! 5. **Agent label interpolates the agent type** as
//!    `Open <agent_type> agent memory`, with no bold escape codes —
//!    styling is a rendering concern.

use crate::memory::open_folder_value::encode_open_folder;
use crate::memory::option_list::MemoryOption;

/// One agent's memory directory + scope label: the agent type, its
/// memory scope, and the pre-resolved directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentMemoryEntry {
    /// Agent type identifier (e.g. `"reviewer"`, `"planner"`).
    /// Rendered into the label as `Open <agent_type> agent memory`.
    pub agent_type: String,
    /// The agent's memory scope (e.g. `"global"`, `"local"`,
    /// `"per-task"`). Rendered as `<memory> scope` in the
    /// description column.
    pub memory: String,
    /// Pre-resolved agent memory directory. Resolving it from the
    /// agent type and memory scope is out of scope for this crate, so
    /// the consumer fills it in.
    pub directory: String,
}

impl AgentMemoryEntry {
    /// Convenience constructor.
    pub fn new(
        agent_type: impl Into<String>,
        memory: impl Into<String>,
        directory: impl Into<String>,
    ) -> Self {
        Self {
            agent_type: agent_type.into(),
            memory: memory.into(),
            directory: directory.into(),
        }
    }
}

/// Inputs for [`build_folder_options`]. The team-memory feature flag
/// and its runtime gate collapse into "team_memory_path is `Some(_)`".
#[derive(Debug, Clone)]
pub struct FolderOptionsInputs<'a> {
    /// Master gate. The entire folder-row block is emitted only when
    /// this is `true`; when it is `false`, the function returns an
    /// empty vec.
    pub auto_memory_enabled: bool,
    /// Auto-memory directory. The consumer resolves it; that lookup
    /// is out of scope for this crate.
    pub auto_memory_path: &'a str,
    /// Team-memory directory, if both the feature flag and the
    /// runtime gate are on. `None` when team-memory is disabled.
    pub team_memory_path: Option<&'a str>,
    /// Active agents with memory directories. The consumer is expected
    /// to have already skipped empty `memory` values — only agents
    /// with a non-empty `memory` and a resolved `directory` should be
    /// passed in.
    pub agents: &'a [AgentMemoryEntry],
}

/// Build the auto-memory / team-memory / agent-memory folder rows.
///
/// Returns an empty vec when `auto_memory_enabled` is `false`.
pub fn build_folder_options(inputs: &FolderOptionsInputs<'_>) -> Vec<MemoryOption> {
    if !inputs.auto_memory_enabled {
        return Vec::new();
    }

    let mut options = Vec::new();

    // Auto-memory row (always first when the gate fires).
    options.push(MemoryOption {
        label: "Open auto-memory folder".to_string(),
        value: encode_open_folder(inputs.auto_memory_path),
        description: String::new(),
    });

    // Team-memory row (when team-memory path is set).
    if let Some(team_path) = inputs.team_memory_path {
        options.push(MemoryOption {
            label: "Open team memory folder".to_string(),
            value: encode_open_folder(team_path),
            description: String::new(),
        });
    }

    // Agent rows (in iteration order).
    for agent in inputs.agents {
        if agent.memory.is_empty() {
            // Skip an empty memory scope: it names no directory.
            continue;
        }
        options.push(MemoryOption {
            label: format!("Open {} agent memory", agent.agent_type),
            value: encode_open_folder(&agent.directory),
            description: format!("{} scope", agent.memory),
        });
    }

    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_agents_inputs<'a>() -> FolderOptionsInputs<'a> {
        FolderOptionsInputs {
            auto_memory_enabled: true,
            auto_memory_path: "/home/u/.rebon/auto-mem",
            team_memory_path: None,
            agents: &[],
        }
    }

    #[test]
    fn disabled_auto_memory_yields_empty_vec() {
        let inputs = FolderOptionsInputs {
            auto_memory_enabled: false,
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert!(opts.is_empty());
    }

    #[test]
    fn disabled_auto_memory_with_team_path_still_yields_empty() {
        let inputs = FolderOptionsInputs {
            auto_memory_enabled: false,
            team_memory_path: Some("/home/u/.rebon/team"),
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert!(opts.is_empty());
    }

    #[test]
    fn enabled_auto_memory_yields_one_row() {
        let opts = build_folder_options(&no_agents_inputs());
        assert_eq!(opts.len(), 1);
    }

    #[test]
    fn auto_memory_row_label_pinned() {
        let opts = build_folder_options(&no_agents_inputs());
        assert_eq!(opts[0].label, "Open auto-memory folder");
    }

    #[test]
    fn auto_memory_row_value_uses_open_folder_prefix() {
        let opts = build_folder_options(&no_agents_inputs());
        assert_eq!(opts[0].value, "__open_folder__/home/u/.rebon/auto-mem");
    }

    #[test]
    fn auto_memory_row_description_is_empty() {
        let opts = build_folder_options(&no_agents_inputs());
        assert_eq!(opts[0].description, "");
    }

    #[test]
    fn team_memory_row_appears_after_auto_memory() {
        let inputs = FolderOptionsInputs {
            team_memory_path: Some("/home/u/.rebon/team"),
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Open auto-memory folder");
        assert_eq!(opts[1].label, "Open team memory folder");
    }

    #[test]
    fn team_memory_row_value_uses_open_folder_prefix() {
        let inputs = FolderOptionsInputs {
            team_memory_path: Some("/home/u/.rebon/team"),
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts[1].value, "__open_folder__/home/u/.rebon/team");
    }

    #[test]
    fn team_memory_row_description_is_empty() {
        let inputs = FolderOptionsInputs {
            team_memory_path: Some("/home/u/.rebon/team"),
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts[1].description, "");
    }

    #[test]
    fn agent_rows_appear_after_auto_memory_and_team_memory() {
        let agents = vec![
            AgentMemoryEntry::new("reviewer", "global", "/agents/reviewer/mem"),
            AgentMemoryEntry::new("planner", "local", "/agents/planner/mem"),
        ];
        let inputs = FolderOptionsInputs {
            team_memory_path: Some("/home/u/.rebon/team"),
            agents: &agents,
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].label, "Open auto-memory folder");
        assert_eq!(opts[1].label, "Open team memory folder");
        assert_eq!(opts[2].label, "Open reviewer agent memory");
        assert_eq!(opts[3].label, "Open planner agent memory");
    }

    #[test]
    fn agent_row_label_format_pinned() {
        let agents = vec![AgentMemoryEntry::new("foobar", "global", "/foobar/mem")];
        let inputs = FolderOptionsInputs {
            agents: &agents,
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts[1].label, "Open foobar agent memory");
    }

    #[test]
    fn agent_row_value_uses_open_folder_prefix() {
        let agents = vec![AgentMemoryEntry::new(
            "foobar",
            "global",
            "/agents/foobar/mem",
        )];
        let inputs = FolderOptionsInputs {
            agents: &agents,
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts[1].value, "__open_folder__/agents/foobar/mem");
    }

    #[test]
    fn agent_row_description_is_scope_label() {
        let agents = vec![AgentMemoryEntry::new(
            "foobar",
            "per-task",
            "/agents/foobar",
        )];
        let inputs = FolderOptionsInputs {
            agents: &agents,
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts[1].description, "per-task scope");
    }

    #[test]
    fn agent_with_empty_memory_is_skipped() {
        // An empty memory string is skipped, so it contributes no entry.
        let agents = vec![
            AgentMemoryEntry::new("with_mem", "global", "/a/with"),
            AgentMemoryEntry::new("no_mem", "", "/a/no"),
            AgentMemoryEntry::new("after_skip", "local", "/a/after"),
        ];
        let inputs = FolderOptionsInputs {
            agents: &agents,
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        // 1 auto + 2 surviving agents = 3
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[1].label, "Open with_mem agent memory");
        assert_eq!(opts[2].label, "Open after_skip agent memory");
    }

    #[test]
    fn agents_in_order_with_no_team_memory() {
        let agents = vec![
            AgentMemoryEntry::new("a1", "global", "/a/1"),
            AgentMemoryEntry::new("a2", "local", "/a/2"),
        ];
        let inputs = FolderOptionsInputs {
            agents: &agents,
            ..no_agents_inputs()
        };
        let opts = build_folder_options(&inputs);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "Open auto-memory folder");
        assert_eq!(opts[1].label, "Open a1 agent memory");
        assert_eq!(opts[2].label, "Open a2 agent memory");
    }

    #[test]
    fn folder_options_table() {
        // (auto_enabled, team_path, agents) → expected (count, labels[])
        struct Case<'a> {
            auto: bool,
            team: Option<&'a str>,
            agents: Vec<AgentMemoryEntry>,
            expected_labels: Vec<&'a str>,
        }
        let cases = [
            Case {
                auto: false,
                team: None,
                agents: vec![],
                expected_labels: vec![],
            },
            Case {
                auto: true,
                team: None,
                agents: vec![],
                expected_labels: vec!["Open auto-memory folder"],
            },
            Case {
                auto: true,
                team: Some("/team"),
                agents: vec![],
                expected_labels: vec!["Open auto-memory folder", "Open team memory folder"],
            },
            Case {
                auto: true,
                team: Some("/team"),
                agents: vec![AgentMemoryEntry::new("alpha", "global", "/a")],
                expected_labels: vec![
                    "Open auto-memory folder",
                    "Open team memory folder",
                    "Open alpha agent memory",
                ],
            },
            Case {
                auto: true,
                team: None,
                agents: vec![
                    AgentMemoryEntry::new("alpha", "global", "/a"),
                    AgentMemoryEntry::new("beta", "", "/b"),
                    AgentMemoryEntry::new("gamma", "local", "/g"),
                ],
                expected_labels: vec![
                    "Open auto-memory folder",
                    "Open alpha agent memory",
                    "Open gamma agent memory",
                ],
            },
            Case {
                auto: false,
                team: Some("/team"),
                agents: vec![AgentMemoryEntry::new("alpha", "global", "/a")],
                expected_labels: vec![],
            },
        ];

        for case in cases {
            let inputs = FolderOptionsInputs {
                auto_memory_enabled: case.auto,
                auto_memory_path: "/auto",
                team_memory_path: case.team,
                agents: &case.agents,
            };
            let opts = build_folder_options(&inputs);
            let labels: Vec<&str> = opts.iter().map(|o| o.label.as_str()).collect();
            assert_eq!(labels, case.expected_labels);
        }
    }
}
