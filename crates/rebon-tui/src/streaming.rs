//! Streaming overlay state (StreamingToolUse / StreamingThinking /
//! StreamingContentBlock / StreamingOverlay + workflow-progress merge helpers).
//!
//! Moved to the framework-agnostic `rebon-render::streaming` so the
//! GPUI app constructs and renders the exact same per-tool input the TUI does
//! (and the workflow-progress merge / `is_workflow_tool_use` alias predicate
//! stay single-source). Re-exported here so every `crate::streaming::…` caller
//! across the render/state modules resolves unchanged.
pub use rebon_render::streaming::*;
