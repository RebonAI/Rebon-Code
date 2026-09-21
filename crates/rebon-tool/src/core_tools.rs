//! The primitive tool set and the facts derived from it.
//!
//! [`core_tool_set`] is the one list of the tools every rebon session has
//! regardless of which features are on: file access, search, the shells, the
//! deferred-tool gateway and `Sleep`. The `core-tools` kernel plugin registers
//! exactly this set on the process seat; an engine that runs without a kernel
//! falls back to the same function, so the two never disagree.
//!
//! [`builtin_tool_facts`] records the core implementations' metadata. Name-only
//! consumers use the neutral `rebon_tools_core::BUILTIN_TOOL_FACTS` table;
//! feature plugins pin their own tools against that table without a reverse dependency.

use std::sync::{Arc, OnceLock};

use rebon_tools_core::{tool_matches_name, ToolKind};

use crate::{
    BashTool, EditTool, GlobTool, GrepTool, InvokeDeferredTool, MultiEditTool, PowerShellTool,
    ReadTool, ShellOutputTool, ShellStopTool, SleepTool, Tool, ToolSearchTool, WriteTool,
};

/// How many tools [`core_tool_set`] returns.
pub const CORE_TOOL_COUNT: usize = 13;

/// The thirteen primitives, in registration order. `Bash` carries a built
/// description, so the caller constructs it; everything else is a unit.
pub fn core_tool_set(bash: BashTool) -> Vec<Arc<dyn Tool>> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool),
        Arc::new(WriteTool),
        Arc::new(EditTool),
        Arc::new(MultiEditTool),
        Arc::new(GlobTool),
        Arc::new(GrepTool),
        Arc::new(bash),
        Arc::new(PowerShellTool),
        Arc::new(ShellOutputTool),
        Arc::new(ShellStopTool),
        Arc::new(ToolSearchTool),
        Arc::new(InvokeDeferredTool),
        Arc::new(SleepTool),
    ];
    debug_assert_eq!(tools.len(), CORE_TOOL_COUNT);
    tools
}

/// What one builtin tool says about itself, recorded once so name-only code
/// can ask without holding the tool.
#[derive(Debug, Clone)]
pub struct ToolFacts {
    pub name: String,
    pub aliases: &'static [&'static str],
    pub kind: ToolKind,
    pub file_target_field: Option<&'static str>,
}

impl ToolFacts {
    pub fn of(tool: &dyn Tool) -> Self {
        Self {
            name: tool.id().as_str().to_string(),
            aliases: tool.aliases(),
            kind: tool.kind(),
            file_target_field: tool.file_target_field(),
        }
    }

    /// Whether `candidate` is this tool's canonical name or one of its aliases.
    pub fn matches(&self, candidate: &str) -> bool {
        tool_matches_name(&self.name, self.aliases, candidate)
    }
}

/// Facts for the core set. Feature plugins pin their own implementations
/// against `rebon_tools_core::BUILTIN_TOOL_FACTS` without a reverse dependency.
/// Built once per process.
pub fn builtin_tool_facts() -> &'static [ToolFacts] {
    static FACTS: OnceLock<Vec<ToolFacts>> = OnceLock::new();
    FACTS.get_or_init(|| {
        core_tool_set(BashTool::new())
            .into_iter()
            .map(|tool| ToolFacts::of(tool.as_ref()))
            .collect()
    })
}

/// The recorded facts for a builtin tool named `name` (canonical or alias).
pub fn facts_for_name(name: &str) -> Option<&'static ToolFacts> {
    builtin_tool_facts()
        .iter()
        .find(|facts| facts.matches(name))
}

/// The policy kind of a builtin tool by name; `Other` for anything the
/// builtins do not know, which is how a plugin tool named `EditDatabase`
/// stays out of the file-edit class.
///
/// Answered from [`rebon_tools_core::BUILTIN_TOOL_FACTS`], which the crates
/// that cannot depend on `rebon-tool` read too, so every surface gives the
/// same answer. [`builtin_tool_kinds_match_the_shared_table`] pins that table
/// against what the tools themselves declare.
pub fn tool_kind_for_name(name: &str) -> ToolKind {
    rebon_tools_core::tool_kind_for_name(name)
}

/// Canonical names of every builtin tool of `kind`, in catalog order.
pub fn tool_names_of_kind(kind: ToolKind) -> Vec<&'static str> {
    rebon_tools_core::tool_names_of_kind(kind)
}

/// The input field a builtin file tool names its target with.
pub fn file_target_field_for_name(name: &str) -> Option<&'static str> {
    rebon_tools_core::file_target_field_for_name(name)
}

/// The rendering kind a policy kind maps to on the ACP wire.
pub fn render_tool_kind(kind: ToolKind) -> rebon_types::ToolKind {
    match kind {
        ToolKind::FileRead => rebon_types::ToolKind::Read,
        ToolKind::FileEdit => rebon_types::ToolKind::Edit,
        ToolKind::Shell => rebon_types::ToolKind::Execute,
        ToolKind::Search => rebon_types::ToolKind::Search,
        ToolKind::Web => rebon_types::ToolKind::Fetch,
        ToolKind::Agent | ToolKind::Task | ToolKind::Other => rebon_types::ToolKind::Other,
    }
}

/// [`render_tool_kind`] for a builtin tool named `name`.
pub fn render_tool_kind_for_name(name: &str) -> rebon_types::ToolKind {
    render_tool_kind(tool_kind_for_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_set_is_thirteen_distinct_names() {
        let tools = core_tool_set(BashTool::new());
        assert_eq!(tools.len(), CORE_TOOL_COUNT);
        let mut names: Vec<String> = tools
            .iter()
            .map(|tool| tool.id().as_str().to_string())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), CORE_TOOL_COUNT);
        for expected in [
            "Read",
            "Write",
            "Edit",
            "MultiEdit",
            "Glob",
            "Grep",
            "Bash",
            "PowerShell",
            "ShellOutput",
            "ShellStop",
            "ToolSearch",
            "InvokeDeferredTool",
            "Sleep",
        ] {
            assert!(names.iter().any(|name| name == expected), "{expected}");
        }
    }

    /// The mirror check. `BUILTIN_TOOL_FACTS` is what every crate that cannot
    /// reach `rebon-tool` reads; this walks the tools themselves and fails if
    /// the two ever say something different — including a kinded builtin that
    /// was added to the core set and forgotten in the table.
    ///
    /// Every kinded tool this crate owns must be in the table; the table may
    /// hold more, because a tool that moved into a feature plugin is still a
    /// builtin and still has a kind. Those rows are pinned the same way from
    /// the plugin crate that owns the tool.
    #[test]
    fn builtin_tool_kinds_match_the_shared_table() {
        let derived: Vec<ToolFacts> = builtin_tool_facts()
            .iter()
            .filter(|facts| facts.kind != ToolKind::Other)
            .cloned()
            .collect();
        let shared = rebon_tools_core::BUILTIN_TOOL_FACTS;
        for derived in &derived {
            let shared = shared
                .iter()
                .find(|entry| entry.name == derived.name)
                .unwrap_or_else(|| {
                    panic!(
                        "{} declares a kind but is missing from the shared table",
                        derived.name
                    )
                });
            assert_eq!(derived.aliases, shared.aliases, "{}", derived.name);
            assert_eq!(derived.kind, shared.kind, "{}", derived.name);
            assert_eq!(
                derived.file_target_field, shared.file_target_field,
                "{}",
                derived.name
            );
        }
    }

    /// The one-time equivalence check: the derived file-edit class
    /// equals what `acceptEdits` and the `Edit(...)` rule umbrella
    /// spell out.
    #[test]
    fn derived_file_edit_class_matches_the_old_list() {
        let mut derived = tool_names_of_kind(ToolKind::FileEdit);
        derived.sort();
        let mut old = vec!["Edit", "Write", "MultiEdit", "NotebookEdit"];
        old.sort();
        assert_eq!(derived, old);
    }

    #[test]
    fn names_resolve_through_aliases_and_unknown_names_are_other() {
        assert_eq!(tool_kind_for_name("FileEditTool"), ToolKind::FileEdit);
        assert_eq!(tool_kind_for_name("FileReadTool"), ToolKind::FileRead);
        assert_eq!(tool_kind_for_name("BashTool"), ToolKind::Shell);
        assert_eq!(tool_kind_for_name("Task"), ToolKind::Agent);
        assert_eq!(tool_kind_for_name("EditDatabase"), ToolKind::Other);
        assert_eq!(
            tool_kind_for_name("mcp__server__edit_file"),
            ToolKind::Other
        );
        assert_eq!(
            file_target_field_for_name("NotebookEdit"),
            Some("notebook_path")
        );
        assert_eq!(
            file_target_field_for_name("MultiEditTool"),
            Some("file_path")
        );
        assert_eq!(file_target_field_for_name("Bash"), None);
    }

    #[test]
    fn render_kinds_follow_policy_kinds() {
        assert_eq!(
            render_tool_kind_for_name("Read"),
            rebon_types::ToolKind::Read
        );
        assert_eq!(
            render_tool_kind_for_name("Grep"),
            rebon_types::ToolKind::Search
        );
        assert_eq!(
            render_tool_kind_for_name("PowerShell"),
            rebon_types::ToolKind::Execute
        );
        assert_eq!(
            render_tool_kind_for_name("WebFetch"),
            rebon_types::ToolKind::Fetch
        );
        assert_eq!(
            render_tool_kind_for_name("Agent"),
            rebon_types::ToolKind::Other
        );
    }
}
