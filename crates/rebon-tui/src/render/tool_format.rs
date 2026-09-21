//! Tool-call header titles + one-line parameter summaries.
//!
//! Moved to the framework-agnostic `rebon-render::summary` so the GPUI
//! app builds identical titles/summaries. Re-exported here so the existing
//! `mod.rs use tool_format::{…}` and `super::streaming_tool_summary` call sites
//! resolve unchanged.

pub(super) use rebon_render::summary::{
    compact_json_map, deferred_tool_verbose_summary, streaming_tool_display_name,
    streaming_tool_summary, truncate_param_value, SUMMARY_MAX_CHARS,
};
