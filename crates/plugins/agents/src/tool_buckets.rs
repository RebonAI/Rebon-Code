//! Which picker bucket a tool belongs in.
//!
//! `ToolKind` is what a tool is to policy; `ToolBucket` is how a picker groups
//! it for a person. The map between them is a plugin-level fact, not a
//! rendering detail: the `/agents` panel is the first surface to need it, and
//! any other one -- the desktop app's agent picker included -- has to group
//! the same tools the same way or the two disagree about what "Edit tools"
//! means.
//!
//! It lives here rather than in [`surface`](crate::surface) because that
//! module takes the classifier as an argument rather than reaching for a tool
//! registry, and this is the argument.

use crate::surface::tool_selector::ToolBucket;
pub use crate::surface::tool_selector::{ToolEntry, ToolSelectorState};

pub fn bucket_for_builtin(name: &str) -> Option<ToolBucket> {
    use rebon_tools_core::ToolKind;
    let Some(facts) = rebon_tools_core::builtin_tool_facts_for_name(name) else {
        return Some(ToolBucket::Other);
    };
    match facts.kind {
        ToolKind::FileRead | ToolKind::Search => Some(ToolBucket::ReadOnly),
        ToolKind::FileEdit => Some(ToolBucket::Edit),
        ToolKind::Shell => Some(ToolBucket::Execution),
        ToolKind::Agent => None,
        ToolKind::Task | ToolKind::Web | ToolKind::Other => Some(ToolBucket::Other),
    }
}

/// A tool-picker state grouped the way this binary groups tools.
///
/// The one entry point front ends use, so that "which bucket" is answered
/// identically everywhere rather than each surface remembering to pass the
/// classifier.
pub fn selector_state(tools: Vec<ToolEntry>, initial: Option<&[String]>) -> ToolSelectorState {
    ToolSelectorState::new(tools, initial, &bucket_for_builtin)
}
