//! Builds the context-usage visualization (categories, legend, grid cells).
//! Token formatting, display-path rendering, and context-suggestion generation
//! remain caller-owned seams.

use std::collections::HashMap;
use std::hash::Hash;

/// Reserved synthetic category name used by the context view.
pub(crate) const RESERVED_CATEGORY_NAME: &str = "Autocompact buffer";

/// Display order for grouped sources shown in the context view.
pub(crate) const SOURCE_DISPLAY_ORDER: [&str; 5] =
    ["Project", "User", "Managed", "Plugin", "Built-in"];

/// Minimal mirror of setting sources used by the context view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SettingSource {
    /// Project-scoped settings.
    Project,
    /// User-scoped settings.
    User,
    /// Managed / policy settings.
    Managed,
    /// Local settings.
    Local,
    /// Flag settings.
    Flag,
}

/// Generic source bucket used by agents / skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SourceKind {
    /// Built from a real setting source.
    Setting(SettingSource),
    /// Plugin-owned entry.
    Plugin,
    /// Built-in entry.
    BuiltIn,
}

/// Category row from `ContextData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextCategory {
    /// Display name.
    pub name: String,
    /// Token count.
    pub tokens: usize,
    /// Theme color key.
    pub color_key: String,
    /// Deferred categories do not count against live context.
    pub is_deferred: bool,
}

/// One square in the usage grid.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GridSquare {
    /// Theme color key.
    pub color_key: String,
    /// Category label.
    pub category_name: String,
    /// Fill ratio (0-1).
    pub square_fullness: f64,
}

/// Glyph choice for one grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GridCellKind {
    /// `Free space` square.
    FreeSpace,
    /// Reserved autocompact square.
    Reserved,
    /// Filled square (`>= 0.7`).
    Full,
    /// Partially filled square (`< 0.7`).
    Partial,
}

/// Planned grid cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GridCellPlan {
    /// Theme color key.
    pub color_key: String,
    /// Glyph choice.
    pub kind: GridCellKind,
}

/// Legend symbol choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegendSymbol {
    /// Free-space legend entry.
    FreeSpace,
    /// Reserved autocompact entry.
    Reserved,
    /// Regular filled category.
    Filled,
}

/// One legend row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegendEntry {
    /// Category name.
    pub name: String,
    /// Theme color key.
    pub color_key: String,
    /// Symbol variant.
    pub symbol: LegendSymbol,
    /// Raw token count.
    pub tokens: usize,
    /// Percent of raw max tokens.
    pub percent_of_raw_max: f64,
}

/// Header summary shown above the grid.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextHeader {
    /// Model name.
    pub model: String,
    /// Current token count.
    pub total_tokens: usize,
    /// Raw model context size.
    pub raw_max_tokens: usize,
    /// Percentage full.
    pub percentage: f64,
}

/// MCP tool detail shown in the context report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpTool {
    /// Tool name.
    pub name: String,
    /// Token count.
    pub tokens: usize,
    /// Whether the tool is already loaded.
    pub is_loaded: bool,
}

/// Deferred built-in tool detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeferredBuiltinTool {
    /// Tool name.
    pub name: String,
    /// Token count.
    pub tokens: usize,
    /// Whether the tool is already loaded.
    pub is_loaded: bool,
}

/// Always-loaded built-in tool detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltinToolDetail {
    /// Tool name.
    pub name: String,
    /// Token count.
    pub tokens: usize,
}

/// System prompt section detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemPromptSectionDetail {
    /// Section name.
    pub name: String,
    /// Token count.
    pub tokens: usize,
}

/// Shared `name + tokens` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedTokens {
    /// Display name.
    pub name: String,
    /// Token count.
    pub tokens: usize,
}

/// Agent row owned by the context display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentDetail {
    /// Agent type.
    pub agent_type: String,
    /// Source bucket.
    pub source: SourceKind,
    /// Token count.
    pub tokens: usize,
}

/// Skill row owned by the context display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillDetail {
    /// Skill name.
    pub name: String,
    /// Source bucket.
    pub source: SourceKind,
    /// Token count.
    pub tokens: usize,
}

/// Skill section summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillInfo {
    /// Total tokens attributed to skills.
    pub tokens: usize,
    /// Individual skill rows.
    pub entries: Vec<SkillDetail>,
}

/// Memory file detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryFileDetail {
    /// Raw path.
    pub path: String,
    /// Token count.
    pub tokens: usize,
}

/// Display-ready memory row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DisplayMemoryFile {
    /// Path after caller-owned display formatting.
    pub display_path: String,
    /// Token count.
    pub tokens: usize,
}

/// Tool-call breakdown entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageToolBreakdown {
    /// Tool name.
    pub name: String,
    /// Tool-call token count.
    pub call_tokens: usize,
    /// Tool-result token count.
    pub result_tokens: usize,
}

/// Attachment breakdown entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageAttachmentBreakdown {
    /// Attachment type name.
    pub name: String,
    /// Token count.
    pub tokens: usize,
}

/// Message breakdown section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageBreakdown {
    /// Tool-call tokens.
    pub tool_call_tokens: usize,
    /// Tool-result tokens.
    pub tool_result_tokens: usize,
    /// Attachment tokens.
    pub attachment_tokens: usize,
    /// Assistant-message tokens.
    pub assistant_message_tokens: usize,
    /// User-message tokens.
    pub user_message_tokens: usize,
    /// Top tools.
    pub tool_calls_by_type: Vec<MessageToolBreakdown>,
    /// Top attachments.
    pub attachments_by_type: Vec<MessageAttachmentBreakdown>,
}

/// Collapse status input captured from the context-collapse runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollapseStatusInput {
    /// Whether context collapse is enabled.
    pub enabled: bool,
    /// Number of collapsed spans.
    pub collapsed_spans: usize,
    /// Number of collapsed messages.
    pub collapsed_messages: usize,
    /// Number of staged spans.
    pub staged_spans: usize,
    /// Total collapse spawns.
    pub total_spawns: usize,
    /// Total collapse errors.
    pub total_errors: usize,
    /// Last error string.
    pub last_error: Option<String>,
    /// Number of consecutive empty spawns.
    pub total_empty_spawns: usize,
    /// Whether the empty-spawn warning fired.
    pub empty_spawn_warning_emitted: bool,
}

/// Render-ready collapse status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollapseStatusPlan {
    /// First status line.
    pub summary: String,
    /// Optional warning line.
    pub warning: Option<String>,
}

/// Generic grouped-source output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceGroup<T> {
    /// Display heading.
    pub display_name: String,
    /// Group members.
    pub items: Vec<T>,
}

/// High-level input for the context visualization helper.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextVisualizationInput {
    /// Header summary.
    pub header: ContextHeader,
    /// Category rows.
    pub categories: Vec<ContextCategory>,
    /// Grid rows.
    pub grid_rows: Vec<Vec<GridSquare>>,
    /// Optional collapse summary.
    pub collapse_status: Option<CollapseStatusInput>,
    /// Memory files.
    pub memory_files: Vec<MemoryFileDetail>,
    /// MCP tools.
    pub mcp_tools: Vec<McpTool>,
    /// Deferred built-in tools.
    pub deferred_builtin_tools: Vec<DeferredBuiltinTool>,
    /// Always-loaded built-in tools.
    pub system_tools: Vec<BuiltinToolDetail>,
    /// System prompt sections.
    pub system_prompt_sections: Vec<SystemPromptSectionDetail>,
    /// Agents.
    pub agents: Vec<AgentDetail>,
    /// Optional skills section.
    pub skills: Option<SkillInfo>,
    /// Optional message breakdown.
    pub message_breakdown: Option<MessageBreakdown>,
    /// Whether the internal-only sections should render.
    pub internal_mode: bool,
}

/// High-level plain-data view plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextVisualizationPlan {
    /// Header summary.
    pub header: ContextHeader,
    /// Optional collapse legend.
    pub collapse_status: Option<CollapseStatusPlan>,
    /// Grid cell plans.
    pub grid: Vec<Vec<GridCellPlan>>,
    /// Visible legend categories.
    pub visible_categories: Vec<LegendEntry>,
    /// Optional free-space legend row.
    pub free_space: Option<LegendEntry>,
    /// Optional autocompact legend row.
    pub autocompact: Option<LegendEntry>,
    /// Whether any MCP entries are deferred.
    pub has_deferred_mcp_tools: bool,
    /// Loaded MCP tools (or all tools when nothing is deferred).
    pub mcp_loaded: Vec<NamedTokens>,
    /// Deferred MCP tool names.
    pub mcp_available: Vec<String>,
    /// Whether any built-in tools are deferred.
    pub has_deferred_builtin_tools: bool,
    /// Loaded system tools.
    pub builtin_loaded: Vec<NamedTokens>,
    /// Deferred system tool names.
    pub builtin_available: Vec<String>,
    /// System prompt sections; empty unless `internal_mode` is set.
    pub system_prompt_sections: Vec<NamedTokens>,
    /// Grouped agents.
    pub agent_groups: Vec<SourceGroup<AgentDetail>>,
    /// Display-formatted memory files.
    pub memory_files: Vec<DisplayMemoryFile>,
    /// Grouped skills.
    pub skill_groups: Vec<SourceGroup<SkillDetail>>,
    /// Message breakdown; `None` unless `internal_mode` is set.
    pub message_breakdown: Option<MessageBreakdown>,
}

fn source_display_name(source: SourceKind) -> &'static str {
    match source {
        SourceKind::Setting(SettingSource::Project) => "Project",
        SourceKind::Setting(SettingSource::User) => "User",
        SourceKind::Setting(SettingSource::Managed) => "Managed",
        SourceKind::Setting(SettingSource::Local) => "Local",
        SourceKind::Setting(SettingSource::Flag) => "Flag",
        SourceKind::Plugin => "Plugin",
        SourceKind::BuiltIn => "Built-in",
    }
}

fn percent(tokens: usize, raw_max_tokens: usize) -> f64 {
    if raw_max_tokens == 0 {
        0.0
    } else {
        (tokens as f64 / raw_max_tokens as f64) * 100.0
    }
}

fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        singular.into()
    } else {
        format!("{singular}s")
    }
}

/// Categories the legend lists: non-empty, not free space, not the
/// autocompact reserve, and not deferred.
pub(crate) fn visible_categories(categories: &[ContextCategory]) -> Vec<ContextCategory> {
    categories
        .iter()
        .filter(|category| {
            category.tokens > 0
                && category.name != "Free space"
                && category.name != RESERVED_CATEGORY_NAME
                && !category.is_deferred
        })
        .cloned()
        .collect()
}

/// Returns the free-space legend row when present.
pub(crate) fn free_space_category(
    categories: &[ContextCategory],
    raw_max_tokens: usize,
) -> Option<LegendEntry> {
    categories
        .iter()
        .find(|category| category.name == "Free space" && category.tokens > 0)
        .map(|category| LegendEntry {
            name: category.name.clone(),
            color_key: category.color_key.clone(),
            symbol: LegendSymbol::FreeSpace,
            tokens: category.tokens,
            percent_of_raw_max: percent(category.tokens, raw_max_tokens),
        })
}

/// Returns the autocompact legend row when present.
pub(crate) fn autocompact_category(
    categories: &[ContextCategory],
    raw_max_tokens: usize,
) -> Option<LegendEntry> {
    categories
        .iter()
        .find(|category| category.name == RESERVED_CATEGORY_NAME && category.tokens > 0)
        .map(|category| LegendEntry {
            name: category.name.clone(),
            color_key: category.color_key.clone(),
            symbol: LegendSymbol::Reserved,
            tokens: category.tokens,
            percent_of_raw_max: percent(category.tokens, raw_max_tokens),
        })
}

/// Picks the glyph for one grid square from its category and fullness.
pub(crate) fn grid_cell_plan(square: &GridSquare) -> GridCellPlan {
    let kind = if square.category_name == "Free space" {
        GridCellKind::FreeSpace
    } else if square.category_name == RESERVED_CATEGORY_NAME {
        GridCellKind::Reserved
    } else if square.square_fullness >= 0.7 {
        GridCellKind::Full
    } else {
        GridCellKind::Partial
    };
    GridCellPlan {
        color_key: square.color_key.clone(),
        kind,
    }
}

/// Builds the collapse-strategy summary line and optional warning, or
/// `None` when collapse is disabled.
pub(crate) fn build_collapse_status(input: &CollapseStatusInput) -> Option<CollapseStatusPlan> {
    if !input.enabled {
        return None;
    }
    let mut parts = Vec::new();
    if input.collapsed_spans > 0 {
        parts.push(format!(
            "{} {} summarized ({} msgs)",
            input.collapsed_spans,
            plural(input.collapsed_spans, "span"),
            input.collapsed_messages
        ));
    }
    if input.staged_spans > 0 {
        parts.push(format!("{} staged", input.staged_spans));
    }
    let summary_suffix = if !parts.is_empty() {
        parts.join(", ")
    } else if input.total_spawns > 0 {
        format!(
            "{} {}, nothing staged yet",
            input.total_spawns,
            plural(input.total_spawns, "spawn")
        )
    } else {
        "waiting for first trigger".into()
    };
    let warning = if input.total_errors > 0 {
        let tail = input
            .last_error
            .as_deref()
            .map(|error| format!(" (last: {})", &error[..error.len().min(60)]))
            .unwrap_or_default();
        Some(format!(
            "Collapse errors: {}/{} spawns failed{}",
            input.total_errors, input.total_spawns, tail
        ))
    } else if input.empty_spawn_warning_emitted {
        Some(format!(
            "Collapse idle: {} consecutive empty runs",
            input.total_empty_spawns
        ))
    } else {
        None
    };
    Some(CollapseStatusPlan {
        summary: format!("Context strategy: collapse ({summary_suffix})"),
        warning,
    })
}

/// Generic source grouping used by agents and skills.
pub(crate) fn grouped_source_items<T, S, N>(
    items: &[T],
    source_of: S,
    tokens_of: N,
) -> Vec<SourceGroup<T>>
where
    T: Clone,
    S: Fn(&T) -> SourceKind,
    N: Fn(&T) -> usize,
{
    let mut groups = HashMap::<String, Vec<T>>::new();
    for item in items {
        let key = source_display_name(source_of(item)).to_string();
        groups.entry(key).or_default().push(item.clone());
    }
    let mut ordered = Vec::new();
    for display_name in SOURCE_DISPLAY_ORDER {
        if let Some(mut group) = groups.remove(display_name) {
            group.sort_by_key(|item| std::cmp::Reverse(tokens_of(item)));
            ordered.push(SourceGroup {
                display_name: display_name.to_string(),
                items: group,
            });
        }
    }
    ordered
}

/// Agents grouped by source, in [`SOURCE_DISPLAY_ORDER`].
pub(crate) fn agent_groups(agents: &[AgentDetail]) -> Vec<SourceGroup<AgentDetail>> {
    grouped_source_items(agents, |agent| agent.source, |agent| agent.tokens)
}

/// Skills grouped by source, in [`SOURCE_DISPLAY_ORDER`].
pub(crate) fn skill_groups(skills: &[SkillDetail]) -> Vec<SourceGroup<SkillDetail>> {
    grouped_source_items(skills, |skill| skill.source, |skill| skill.tokens)
}

/// Pure builder for the [`ContextVisualizationPlan`] the context report renders.
pub(crate) fn build_context_visualization<F>(
    input: &ContextVisualizationInput,
    display_path: F,
) -> ContextVisualizationPlan
where
    F: Fn(&str) -> String,
{
    let has_deferred_mcp_tools = input
        .categories
        .iter()
        .any(|category| category.is_deferred && category.name.contains("MCP"));
    let has_deferred_builtin_tools = !input.deferred_builtin_tools.is_empty();
    let visible_categories = visible_categories(&input.categories)
        .into_iter()
        .map(|category| LegendEntry {
            name: category.name,
            color_key: category.color_key,
            symbol: LegendSymbol::Filled,
            tokens: category.tokens,
            percent_of_raw_max: percent(category.tokens, input.header.raw_max_tokens),
        })
        .collect::<Vec<_>>();
    let mcp_loaded = if has_deferred_mcp_tools {
        input
            .mcp_tools
            .iter()
            .filter(|tool| tool.is_loaded)
            .map(|tool| NamedTokens {
                name: tool.name.clone(),
                tokens: tool.tokens,
            })
            .collect()
    } else {
        input
            .mcp_tools
            .iter()
            .map(|tool| NamedTokens {
                name: tool.name.clone(),
                tokens: tool.tokens,
            })
            .collect()
    };
    let mcp_available = if has_deferred_mcp_tools {
        input
            .mcp_tools
            .iter()
            .filter(|tool| !tool.is_loaded)
            .map(|tool| tool.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    let builtin_loaded = if input.internal_mode {
        input
            .system_tools
            .iter()
            .map(|tool| NamedTokens {
                name: tool.name.clone(),
                tokens: tool.tokens,
            })
            .chain(
                input
                    .deferred_builtin_tools
                    .iter()
                    .filter(|tool| tool.is_loaded)
                    .map(|tool| NamedTokens {
                        name: tool.name.clone(),
                        tokens: tool.tokens,
                    }),
            )
            .collect()
    } else {
        Vec::new()
    };
    let builtin_available = if input.internal_mode && has_deferred_builtin_tools {
        input
            .deferred_builtin_tools
            .iter()
            .filter(|tool| !tool.is_loaded)
            .map(|tool| tool.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    let system_prompt_sections = if input.internal_mode {
        input
            .system_prompt_sections
            .iter()
            .map(|section| NamedTokens {
                name: section.name.clone(),
                tokens: section.tokens,
            })
            .collect()
    } else {
        Vec::new()
    };
    let memory_files = input
        .memory_files
        .iter()
        .map(|file| DisplayMemoryFile {
            display_path: display_path(&file.path),
            tokens: file.tokens,
        })
        .collect();
    ContextVisualizationPlan {
        header: input.header.clone(),
        collapse_status: input
            .collapse_status
            .as_ref()
            .and_then(build_collapse_status),
        grid: input
            .grid_rows
            .iter()
            .map(|row| row.iter().map(grid_cell_plan).collect())
            .collect(),
        visible_categories,
        free_space: free_space_category(&input.categories, input.header.raw_max_tokens),
        autocompact: autocompact_category(&input.categories, input.header.raw_max_tokens),
        has_deferred_mcp_tools,
        mcp_loaded,
        mcp_available,
        has_deferred_builtin_tools,
        builtin_loaded,
        builtin_available,
        system_prompt_sections,
        agent_groups: agent_groups(&input.agents),
        memory_files,
        skill_groups: input
            .skills
            .as_ref()
            .filter(|skills| skills.tokens > 0)
            .map(|skills| skill_groups(&skills.entries))
            .unwrap_or_default(),
        message_breakdown: if input.internal_mode {
            input.message_breakdown.clone()
        } else {
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn category(name: &str, tokens: usize, color_key: &str, is_deferred: bool) -> ContextCategory {
        ContextCategory {
            name: name.into(),
            tokens,
            color_key: color_key.into(),
            is_deferred,
        }
    }

    #[test]
    fn visible_categories_match_filter_rules() {
        let visible = visible_categories(&[
            category("Free space", 10, "dim", false),
            category(RESERVED_CATEGORY_NAME, 10, "reserved", false),
            category("Deferred MCP", 10, "blue", true),
            category("Messages", 0, "red", false),
            category("Tools", 50, "green", false),
        ]);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].name, "Tools");
    }

    #[test]
    fn collapse_status_builds_summary_and_warning() {
        let plan = build_collapse_status(&CollapseStatusInput {
            enabled: true,
            collapsed_spans: 2,
            collapsed_messages: 10,
            staged_spans: 1,
            total_spawns: 4,
            total_errors: 1,
            last_error: Some(
                "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".into(),
            ),
            total_empty_spawns: 0,
            empty_spawn_warning_emitted: false,
        })
        .expect("plan");
        assert!(plan.summary.contains("2 spans summarized"));
        assert!(plan.summary.contains("1 staged"));
        assert!(plan
            .warning
            .as_deref()
            .is_some_and(|line| line.contains("last: abcdef")));
    }

    #[test]
    fn grid_cell_kind_follows_thresholds() {
        assert_eq!(
            grid_cell_plan(&GridSquare {
                color_key: "x".into(),
                category_name: "Free space".into(),
                square_fullness: 0.1,
            })
            .kind,
            GridCellKind::FreeSpace
        );
        assert_eq!(
            grid_cell_plan(&GridSquare {
                color_key: "x".into(),
                category_name: RESERVED_CATEGORY_NAME.into(),
                square_fullness: 0.1,
            })
            .kind,
            GridCellKind::Reserved
        );
        assert_eq!(
            grid_cell_plan(&GridSquare {
                color_key: "x".into(),
                category_name: "Tools".into(),
                square_fullness: 0.8,
            })
            .kind,
            GridCellKind::Full
        );
    }

    #[test]
    fn grouped_sources_follow_display_order_and_token_sort() {
        let groups = grouped_source_items(
            &[
                AgentDetail {
                    agent_type: "local".into(),
                    source: SourceKind::Setting(SettingSource::Local),
                    tokens: 999,
                },
                AgentDetail {
                    agent_type: "b".into(),
                    source: SourceKind::Setting(SettingSource::Project),
                    tokens: 10,
                },
                AgentDetail {
                    agent_type: "a".into(),
                    source: SourceKind::Setting(SettingSource::Project),
                    tokens: 20,
                },
                AgentDetail {
                    agent_type: "plugin".into(),
                    source: SourceKind::Plugin,
                    tokens: 5,
                },
            ],
            |item| item.source,
            |item| item.tokens,
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].display_name, "Project");
        assert_eq!(groups[0].items[0].agent_type, "a");
        assert_eq!(groups[1].display_name, "Plugin");
    }

    #[test]
    fn build_context_visualization_partitions_sections() {
        let plan = build_context_visualization(
            &ContextVisualizationInput {
                header: ContextHeader {
                    model: "haiku".into(),
                    total_tokens: 120,
                    raw_max_tokens: 200,
                    percentage: 60.0,
                },
                categories: vec![
                    category("Tools", 80, "green", false),
                    category("Free space", 20, "dim", false),
                    category(RESERVED_CATEGORY_NAME, 10, "yellow", false),
                    category("MCP deferred", 5, "blue", true),
                ],
                grid_rows: vec![vec![GridSquare {
                    color_key: "green".into(),
                    category_name: "Tools".into(),
                    square_fullness: 1.0,
                }]],
                collapse_status: None,
                memory_files: vec![MemoryFileDetail {
                    path: "/tmp/CLAUDE.md".into(),
                    tokens: 12,
                }],
                mcp_tools: vec![
                    McpTool {
                        name: "loaded".into(),
                        tokens: 10,
                        is_loaded: true,
                    },
                    McpTool {
                        name: "available".into(),
                        tokens: 10,
                        is_loaded: false,
                    },
                ],
                deferred_builtin_tools: vec![
                    DeferredBuiltinTool {
                        name: "deferred".into(),
                        tokens: 5,
                        is_loaded: false,
                    },
                    DeferredBuiltinTool {
                        name: "loaded-deferred".into(),
                        tokens: 6,
                        is_loaded: true,
                    },
                ],
                system_tools: vec![BuiltinToolDetail {
                    name: "core".into(),
                    tokens: 4,
                }],
                system_prompt_sections: vec![SystemPromptSectionDetail {
                    name: "rules".into(),
                    tokens: 7,
                }],
                agents: vec![AgentDetail {
                    agent_type: "worker".into(),
                    source: SourceKind::Setting(SettingSource::Project),
                    tokens: 9,
                }],
                skills: Some(SkillInfo {
                    tokens: 5,
                    entries: vec![SkillDetail {
                        name: "lint".into(),
                        source: SourceKind::Plugin,
                        tokens: 5,
                    }],
                }),
                message_breakdown: Some(MessageBreakdown {
                    tool_call_tokens: 1,
                    tool_result_tokens: 2,
                    attachment_tokens: 3,
                    assistant_message_tokens: 4,
                    user_message_tokens: 5,
                    tool_calls_by_type: vec![MessageToolBreakdown {
                        name: "Read".into(),
                        call_tokens: 1,
                        result_tokens: 2,
                    }],
                    attachments_by_type: vec![MessageAttachmentBreakdown {
                        name: "png".into(),
                        tokens: 3,
                    }],
                }),
                internal_mode: true,
            },
            |path| format!("display:{path}"),
        );
        assert_eq!(plan.visible_categories.len(), 1);
        assert!(plan.has_deferred_mcp_tools);
        assert_eq!(plan.mcp_loaded.len(), 1);
        assert_eq!(plan.mcp_available, vec!["available"]);
        assert_eq!(plan.builtin_loaded.len(), 2);
        assert_eq!(plan.builtin_available, vec!["deferred"]);
        assert_eq!(plan.system_prompt_sections.len(), 1);
        assert_eq!(plan.memory_files[0].display_path, "display:/tmp/CLAUDE.md");
        assert_eq!(plan.skill_groups.len(), 1);
        assert!(plan.message_breakdown.is_some());
        assert!(plan.autocompact.is_some());
        assert!(plan.free_space.is_some());
    }
}
