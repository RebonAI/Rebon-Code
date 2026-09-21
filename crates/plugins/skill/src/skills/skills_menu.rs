//! Projection helpers behind the skills menu: source titles, command
//! filtering, and grouping into rows.

use std::collections::BTreeMap;

/// Sources grouped by the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillSource {
    /// Project settings directory.
    ProjectSettings,
    /// User settings directory.
    UserSettings,
    /// Policy-managed settings.
    PolicySettings,
    /// Local (gitignored) project settings.
    LocalSettings,
    /// Settings supplied on the command line.
    FlagSettings,
    /// A plugin.
    Plugin,
    /// An MCP server.
    Mcp,
}

/// Loading source used to decide whether command paths should appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLoadedFrom {
    /// Loaded from the skills directory.
    Skills,
    /// Loaded from deprecated command files.
    CommandsDeprecated,
    /// Loaded from a plugin.
    Plugin,
    /// Loaded from MCP.
    Mcp,
}

/// Minimal skill-command shape consumed by [`project_skills_menu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    /// Command name displayed for the skill.
    pub command_name: String,
    /// Raw skill name (`<server>:<skill>` for MCP).
    pub name: String,
    /// Group source.
    pub source: SkillSource,
    /// Where the skill was loaded from.
    pub loaded_from: SkillLoadedFrom,
    /// Optional plugin manifest name.
    pub plugin_name: Option<String>,
    /// Estimated frontmatter tokens.
    pub estimated_tokens: u64,
}

/// Input bag for the menu projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMenuInput {
    /// Candidate commands.
    pub commands: Vec<SkillCommand>,
    /// Source -> skills path used for file-based subtitles.
    pub skills_paths: BTreeMap<SkillSource, String>,
    /// Source -> commands path used when a skill came from the deprecated
    /// commands directory ([`SkillLoadedFrom::CommandsDeprecated`]).
    pub commands_paths: BTreeMap<SkillSource, String>,
}

/// Group projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillGroupProjection {
    /// Group title.
    pub title: String,
    /// Optional group subtitle.
    pub subtitle: Option<String>,
    /// Rows inside the group.
    pub skills: Vec<SkillRowProjection>,
}

/// Row projection for one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRowProjection {
    /// Primary display text.
    pub command_name: String,
    /// Optional plugin name suffix.
    pub plugin_name: Option<String>,
    /// `~N description tokens`
    pub token_display: String,
}

/// Whole-menu projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillMenuProjection {
    /// Empty-state dialog.
    Empty,
    /// Non-empty grouped dialog.
    Groups {
        /// Dialog subtitle like `3 skills`.
        subtitle: String,
        /// Ordered groups.
        groups: Vec<SkillGroupProjection>,
    },
}

/// Returns the skill commands that should be considered by the menu.
/// Inputs are already pre-filtered by the caller, so this preserves order.
pub fn filter_skill_commands(commands: &[SkillCommand]) -> Vec<SkillCommand> {
    commands.to_vec()
}

/// Group title for a source (`Plugin skills`, `User settings skills`, …).
pub fn get_source_title(source: SkillSource) -> String {
    match source {
        SkillSource::Plugin => "Plugin skills".to_string(),
        SkillSource::Mcp => "MCP skills".to_string(),
        SkillSource::ProjectSettings => "Project settings skills".to_string(),
        SkillSource::UserSettings => "User settings skills".to_string(),
        SkillSource::PolicySettings => "Policy settings skills".to_string(),
        SkillSource::LocalSettings => "Local settings skills".to_string(),
        SkillSource::FlagSettings => "Flag settings skills".to_string(),
    }
}

/// Group subtitle for a source: the MCP server names for MCP skills,
/// otherwise the skills path, followed by the commands path when any
/// skill in the group came from the deprecated commands directory.
pub fn get_source_subtitle(
    source: SkillSource,
    skills: &[SkillCommand],
    skills_path: Option<&str>,
    commands_path: Option<&str>,
) -> Option<String> {
    if source == SkillSource::Mcp {
        let mut servers = Vec::<String>::new();
        for skill in skills {
            if let Some((server, _)) = skill.name.split_once(':') {
                if !servers.iter().any(|s| s == server) {
                    servers.push(server.to_string());
                }
            }
        }
        return if servers.is_empty() {
            None
        } else {
            Some(servers.join(", "))
        };
    }

    let skills_path = skills_path?;
    let has_commands_skills = skills
        .iter()
        .any(|skill| skill.loaded_from == SkillLoadedFrom::CommandsDeprecated);
    if has_commands_skills {
        commands_path.map(|commands| format!("{skills_path}, {commands}"))
    } else {
        Some(skills_path.to_string())
    }
}

/// Projects the whole skills menu.
pub fn project_skills_menu(input: &SkillMenuInput) -> SkillMenuProjection {
    let skills = filter_skill_commands(&input.commands);
    if skills.is_empty() {
        return SkillMenuProjection::Empty;
    }

    let mut groups: BTreeMap<SkillSource, Vec<SkillCommand>> = BTreeMap::new();
    for skill in skills {
        groups.entry(skill.source).or_default().push(skill);
    }
    for group in groups.values_mut() {
        group.sort_by(|a, b| a.command_name.cmp(&b.command_name));
    }

    let ordered_sources = [
        SkillSource::ProjectSettings,
        SkillSource::UserSettings,
        SkillSource::PolicySettings,
        SkillSource::LocalSettings,
        SkillSource::FlagSettings,
        SkillSource::Plugin,
        SkillSource::Mcp,
    ];
    let projected_groups = ordered_sources
        .into_iter()
        .filter_map(|source| {
            let group_skills = groups.get(&source)?;
            if group_skills.is_empty() {
                return None;
            }
            Some(SkillGroupProjection {
                title: get_source_title(source),
                subtitle: get_source_subtitle(
                    source,
                    group_skills,
                    input.skills_paths.get(&source).map(String::as_str),
                    input.commands_paths.get(&source).map(String::as_str),
                ),
                skills: group_skills
                    .iter()
                    .map(|skill| SkillRowProjection {
                        command_name: skill.command_name.clone(),
                        plugin_name: if source == SkillSource::Plugin {
                            skill.plugin_name.clone()
                        } else {
                            None
                        },
                        token_display: format!(
                            "~{} description tokens",
                            format_tokens(skill.estimated_tokens)
                        ),
                    })
                    .collect(),
            })
        })
        .collect::<Vec<_>>();

    let count = input.commands.len();
    SkillMenuProjection::Groups {
        subtitle: format!("{count} {}", plural(count, "skill")),
        groups: projected_groups,
    }
}

fn format_tokens(count: u64) -> String {
    if count >= 1000 {
        let value = count as f64 / 1000.0;
        let mut text = format!("{value:.1}");
        if text.ends_with(".0") {
            text.truncate(text.len() - 2);
        }
        format!("{text}k")
    } else {
        count.to_string()
    }
}

fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        singular.to_string()
    } else {
        format!("{singular}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(
        command_name: &str,
        name: &str,
        source: SkillSource,
        loaded_from: SkillLoadedFrom,
    ) -> SkillCommand {
        SkillCommand {
            command_name: command_name.into(),
            name: name.into(),
            source,
            loaded_from,
            plugin_name: None,
            estimated_tokens: 1234,
        }
    }

    #[test]
    fn source_titles_are_expected() {
        assert_eq!(get_source_title(SkillSource::Plugin), "Plugin skills");
        assert_eq!(get_source_title(SkillSource::Mcp), "MCP skills");
        assert_eq!(
            get_source_title(SkillSource::UserSettings),
            "User settings skills"
        );
    }

    #[test]
    fn mcp_subtitle_dedupes_servers() {
        let skills = vec![
            skill("a", "server1:foo", SkillSource::Mcp, SkillLoadedFrom::Mcp),
            skill("b", "server1:bar", SkillSource::Mcp, SkillLoadedFrom::Mcp),
            skill("c", "server2:baz", SkillSource::Mcp, SkillLoadedFrom::Mcp),
        ];
        assert_eq!(
            get_source_subtitle(SkillSource::Mcp, &skills, None, None),
            Some("server1, server2".into())
        );
    }

    #[test]
    fn file_subtitle_includes_commands_path_when_deprecated_commands_present() {
        let skills = vec![skill(
            "a",
            "skill-a",
            SkillSource::ProjectSettings,
            SkillLoadedFrom::CommandsDeprecated,
        )];
        assert_eq!(
            get_source_subtitle(
                SkillSource::ProjectSettings,
                &skills,
                Some(".rebon/skills"),
                Some(".rebon/commands")
            ),
            Some(".rebon/skills, .rebon/commands".into())
        );
    }

    #[test]
    fn empty_projection_returns_empty_state() {
        let projection = project_skills_menu(&SkillMenuInput {
            commands: Vec::new(),
            skills_paths: BTreeMap::new(),
            commands_paths: BTreeMap::new(),
        });
        assert_eq!(projection, SkillMenuProjection::Empty);
    }

    #[test]
    fn group_projection_sorts_rows_and_builds_subtitle() {
        let mut skills_paths = BTreeMap::new();
        skills_paths.insert(SkillSource::ProjectSettings, ".rebon/skills".into());

        let projection = project_skills_menu(&SkillMenuInput {
            commands: vec![
                skill(
                    "zeta",
                    "skill-z",
                    SkillSource::ProjectSettings,
                    SkillLoadedFrom::Skills,
                ),
                skill(
                    "alpha",
                    "skill-a",
                    SkillSource::ProjectSettings,
                    SkillLoadedFrom::Skills,
                ),
            ],
            skills_paths,
            commands_paths: BTreeMap::new(),
        });
        let SkillMenuProjection::Groups { subtitle, groups } = projection else {
            panic!("expected groups");
        };
        assert_eq!(subtitle, "2 skills");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].skills[0].command_name, "alpha");
        assert_eq!(groups[0].skills[1].command_name, "zeta");
    }

    #[test]
    fn plugin_row_threads_plugin_name_and_token_display() {
        let mut skill = skill(
            "alpha",
            "plugin-skill",
            SkillSource::Plugin,
            SkillLoadedFrom::Plugin,
        );
        skill.plugin_name = Some("Plugin A".into());
        let projection = project_skills_menu(&SkillMenuInput {
            commands: vec![skill],
            skills_paths: BTreeMap::new(),
            commands_paths: BTreeMap::new(),
        });
        let SkillMenuProjection::Groups { groups, .. } = projection else {
            panic!("expected groups");
        };
        assert_eq!(groups[0].skills[0].plugin_name.as_deref(), Some("Plugin A"));
        assert_eq!(
            groups[0].skills[0].token_display,
            "~1.2k description tokens"
        );
    }
}
