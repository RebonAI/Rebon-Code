//! Canonical tool-name → `ToolKind` classification.
//!
//! The single source of truth for tool-name → kind, shared by every consumer.
//! The mapping is deliberately a union rather than a minimum: `NotebookRead` /
//! `List` → Read, `BashOutput` → Execute, `WebSearch` → Search and
//! `NotebookEdit` (already Edit) are classified from the specific name where one
//! exists, so a name only reaches `Other` when nothing recognises it.

use rebon_types::ToolKind;

/// How much of a tool's output a renderer shows.
///
/// * **Compact**: header + a short body preview (the default).
/// * **Normal**: header + first lines, no aggressive truncation.
/// * **Verbose**: full details — all content, locations, raw output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolOutputVerbosity {
    #[default]
    Compact,
    Normal,
    Verbose,
}

pub fn tool_kind_from_name(name: &str) -> ToolKind {
    // Whoever writes a file draws as an edit; the tools say who that is, so
    // the renderer covers a new editor (or an alias) without being told.
    if rebon_tools_core::tool_kind_for_name(name) == rebon_tools_core::ToolKind::FileEdit {
        return ToolKind::Edit;
    }
    match name {
        "Read" | "NotebookRead" | "Glob" | "LS" | "List" => ToolKind::Read,
        "Grep" | "WebSearch" => ToolKind::Search,
        "Bash" | "PowerShell" | "BashOutput" | "ShellOutput" | "ShellStop" => ToolKind::Execute,
        "WebFetch" | "Fetch" => ToolKind::Fetch,
        "Delete" => ToolKind::Delete,
        "Move" => ToolKind::Move,
        "Think" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}
