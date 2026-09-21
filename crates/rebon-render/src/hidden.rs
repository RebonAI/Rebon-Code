//! Tools that render NO transcript card (they are noise / coordinator-internal).
//!
//! The single list every surface gates on, so a tool added here disappears
//! from all of them at once.

/// Tool names whose tool-call rows are suppressed in the transcript:
/// - `TodoWrite` / `Task*`: the pinned plan strip already shows this state.
/// - `AskUserQuestion` / `Enter|ExitPlanMode`: rendered by dedicated surfaces.
/// - `ToolSearch`: deferred-tool gateway noise.
/// - `Team*` / `SyntheticOutput`: coordinator-internal, never user-facing.
pub const TRANSCRIPT_HIDDEN_TOOLS: &[&str] = &[
    "TodoWrite",
    "TaskCreate",
    "TaskUpdate",
    "TaskList",
    "TaskGet",
    "TaskStop",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "ToolSearch",
    "TeamCreate",
    "TeamDelete",
    "SyntheticOutput",
];

/// Whether a tool name renders no transcript card.
pub fn is_transcript_hidden_tool(name: &str) -> bool {
    TRANSCRIPT_HIDDEN_TOOLS.iter().any(|&hidden| hidden == name)
}
