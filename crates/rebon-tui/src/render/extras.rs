use super::*;

/// Live status shown for a background Agent tool card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAgentToolActivity {
    pub text: Option<String>,
    pub status: LiveAgentToolStatus,
    pub title: Option<String>,
    pub display_name: Option<String>,
    pub start_time_ms: Option<u64>,
    pub end_time_ms: Option<u64>,
    pub tool_use_count: Option<u64>,
    pub token_count: Option<u64>,
    pub terminal_result: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiveAgentToolStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

/// Render-time overlays supplied by the CLI layer.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptRenderExtras<'a> {
    pub live_agent_tool_activity: &'a HashMap<String, LiveAgentToolActivity>,
    pub live_activity_revision: u64,
    /// Tool call ids that rebon's auto mode let through without a permission
    /// dialog, mapped to which part of the gate decided. Session-scoped and
    /// display-only: the CLI fills it from the live update stream and from the
    /// transcript's `autoModeAllowed` sidecar, and tool cards in it get an
    /// extra note row naming the source.
    pub auto_mode_allowed_tool_ids:
        &'a std::collections::HashMap<String, rebon_types::AutoModeAllowSource>,
    /// When a caller renders only a live suffix of the transcript (inline mode
    /// after older rows have been inserted into terminal scrollback), the first
    /// visible segment can still be a continuation after a previous prompt. Set
    /// this to preserve the normal inter-segment margin at that boundary
    /// without changing full-screen transcript rendering.
    pub leading_segment_margin: bool,
    /// Inline committed scrollback must keep visible reasoning that has been
    /// progressively flushed as its own assistant row. Screen/default rendering
    /// keeps these rows transparent so hidden-tool residue does not fragment
    /// compact tool collapse groups.
    pub render_thinking_only_rows: bool,
    /// Expand thinking rows while leaving the rest of the transcript on the
    /// requested verbosity. Callers can use this to reveal full thinking
    /// without also expanding read/search tool records.
    pub expand_thinking_rows: bool,
    /// Keep compact Edit/Write/Update previews from falling back to the
    /// first-lines-only cap. Long runs still use the shared prompt-style fold;
    /// only explicit verbose mode expands them completely.
    pub force_verbose_edit_tool_previews: bool,
    /// Render committed Agent groups for immutable terminal scrollback: use a
    /// static gutter and omit the placeholder activity row until real activity exists.
    pub static_agent_group_status: bool,
    /// Inline live tail only: bound an in-progress Workflow card to at most
    /// this many terminal rows by eliding the middle of its body (header and
    /// run summary stay, the freshest rows stay). The inline live region is
    /// bottom-anchored and in-progress blocks cannot drain, so an unbounded
    /// live card would push its own header above the viewport where it is
    /// neither visible nor in scrollback. `None` (screen mode, committed
    /// scrollback inserts, terminal cards) renders the full card.
    pub inline_live_workflow_card_max_rows: Option<u16>,
}

impl<'a> TranscriptRenderExtras<'a> {
    pub fn empty() -> Self {
        Self {
            live_agent_tool_activity: &EMPTY_LIVE_AGENT_TOOL_ACTIVITY,
            live_activity_revision: 0,
            auto_mode_allowed_tool_ids: &EMPTY_AUTO_MODE_ALLOWED_TOOL_IDS,
            leading_segment_margin: false,
            render_thinking_only_rows: false,
            expand_thinking_rows: false,
            force_verbose_edit_tool_previews: false,
            static_agent_group_status: false,
            inline_live_workflow_card_max_rows: None,
        }
    }
}

pub(super) static EMPTY_AUTO_MODE_ALLOWED_TOOL_IDS: std::sync::LazyLock<
    std::collections::HashMap<String, rebon_types::AutoModeAllowSource>,
> = std::sync::LazyLock::new(std::collections::HashMap::new);

pub(super) static EMPTY_LIVE_AGENT_TOOL_ACTIVITY: std::sync::LazyLock<
    HashMap<String, LiveAgentToolActivity>,
> = std::sync::LazyLock::new(HashMap::new);
