//! # rebon-tui — terminal transcript and message rendering
//!
//! This crate provides ratatui rendering for transcript rows, message
//! content blocks, streaming overlays, prompt input, and related
//! measurement/state helpers. The rendering code is covered by tests
//! that pin message-shape compatibility and terminal buffer output.
//!
//! ## Modules
//!
//! * [`message`] — wire-compatible transcript `Message` data model
//!   (user / assistant / attachment / system, with nested Anthropic
//!   content blocks). Deserializable from real JSON fixtures.
//! * [`transcript`] — flat append-only store of committed `Message`
//!   values.
//! * [`streaming`] — overlay state for streaming text, tool uses,
//!   and thinking, with ACP
//!   tool-call lifecycle metadata layered onto streaming tool entries
//!   for the local TUI bridge.
//! * [`state`] — reducer with cancel-commit ordering for partial
//!   assistant output.
//! * [`user_text`] — XML wrapper classifier with balanced-pair tag
//!   extraction and the `NO_CONTENT_MESSAGE` sentinel.
//! * [`render`] — ratatui renderer for transcript/message dispatch.
//!   Display-width-aware wrap via `unicode-width`.
//! * [`measure`] — `(uuid, width)`-keyed height cache with width-change
//!   invalidation.
//!
//! ## Removed internal prototypes
//!
//! An earlier cut of this crate shipped `viewport.rs`,
//! `pipeline.rs`, `layout.rs`, and `tool_render.rs`. A hardening
//! audit found each of them substantially misaligned with how the
//! transcript actually behaves:
//!
//! * **`viewport.rs`** invented a pure-function `ScrollAnchor`
//!   resolver. The needed scroll behavior is imperative: the scroll
//!   offset lives on the scroll container, sticky scrolling is a
//!   separate flag, the anchor is an element plus offset resolved at
//!   paint time, pending deltas accumulate for rate limiting, and the
//!   clamp bounds are explicit setters.
//!   A complete Rust implementation would need a real
//!   scroll subsystem with its own layout resolution step — out of scope for
//!   transcript rendering.
//! * **`pipeline.rs`** invented pass names (`FilterZeroHeight`,
//!   `CoalesceAssistantText`, `NormalizeToolPairs`) that don't
//!   match the production pipeline. The real passes normalize the
//!   messages, drop empty ones, keep only what follows the last compact
//!   boundary, drop progress rows, attachments that render nothing and
//!   hidden user messages, reorder, apply the brief filter, truncate,
//!   group, collapse read/search groups, teammate shutdowns, hook
//!   summaries and background bash notifications, and finally build
//!   the message lookups.
//!   Each pass is substantial; none are implemented here.
//! * **`layout.rs`** invented an `UnseenDividerState` with a
//!   "1 new" floor and assistant-turn-only counting. The real
//!   unseen-divider rules
//!   (divider-position race ordering, modal pane suppression, assistant-
//!   turn semantics) are implemented in [`layout`] with
//!   positive/negative/edge tests. The terminal runner wires
//!   `layout_zones()` from that module.
//! * **`tool_render.rs`** invented a `ToolRender` trait +
//!   `ToolRenderRegistry`. Per-tool rendering belongs with each tool
//!   (how its use and its result render), not in a registry owned by
//!   this crate.
//!
//! These modules were removed rather than carried as "stubs"
//! because a misaligned stub has negative value: it hints at a
//! wrong architecture and forces downstream code to either preserve
//! the mistake or do a disruptive rename.
//!
//! Each deleted concern has a clear landing path, and the types this
//! crate exports have been chosen to NOT force future work into a
//! specific shape.
//!
//! ## Pure-logic crate integration
//!
//! This crate consumes several state-machine crates:
//!
//! * [`rebon_design_system`] — theme palette: `RenderTheme::from_theme_name()`
//!   derives ratatui styles from the design-system's 90+ color keys.
//! * [`rebon_render`] — the markdown-syntax fast-path detection
//!   (`has_markdown_syntax()`) a caller can use to skip fence scanning for
//!   plain text.
//!
//! ## Merged-in pure-logic modules
//!
//! Crates whose only consumers were this crate's terminal half now live
//! here as modules rather than as their own
//! Cargo entries. None of them names ratatui, and none should start:
//!
//! * [`layout`] — fullscreen layout zone projection, the unseen-divider
//!   state machine and the new-messages pill label.
//! * [`input`] — the placeholder decision tree
//!   (dim / inverse-first-char / voice cursor / hidden modes) consumed
//!   by `render_prompt_input()`, plus the paste-return gate, the
//!   highlight-viewport remap and the vim-mode sync predicate.
//! * [`promptinput`] — the prompt surface's state machines: paste flow,
//!   submit flow, footer navigation, queue display, mode cycling.
//! * [`status`] — the footer and status-row projections the prompt
//!   surface composes: token warning, memory badge, status-line gate
//!   (its `effort_indicator` went to
//!   `rebon_types` instead, because headless CLI paths read it too).

pub mod dialog_view;
pub mod input;
pub mod layout;
pub mod measure;
pub mod message;
pub mod promptinput;
pub mod render;
pub mod render_prompt_input;
pub mod selection;
pub mod state;
pub mod status;
pub mod streaming;
pub mod transcript;
pub mod user_text;

pub use measure::{MeasureCache, MeasureKey};
pub use message::{
    is_not_empty_message, AssistantContentBlock, AssistantMessage, AssistantMessageInner,
    AssistantRedactedThinkingBlock, AssistantRole, AssistantTextBlock, AssistantThinkingBlock,
    AssistantToolUseBlock, AttachmentRow, Message, SystemLevel, SystemMessage, ToolResultContent,
    UserContentBlock, UserImageBlock, UserMessage, UserMessageInner, UserRole, UserTextBlock,
    UserToolResultBlock,
};
pub use render::{
    parse_theme_color, render_message, render_streaming_overlay, render_transcript,
    render_transcript_cached, render_transcript_cached_with_running_hints,
    trailing_collapsible_tool_run_start, LiveAgentToolActivity, LiveAgentToolStatus,
    MathDisplayMode, MathGraphicsProtocol, RenderTheme, ToolOutputVerbosity,
    TranscriptMeasureCache, TranscriptRenderExtras, TranscriptRenderResult, TranscriptStickyAnchor,
};
pub use render_prompt_input::{
    layout_prompt_input_line, render_prompt_input, PromptInputLineLayout, PromptInputRenderResult,
};
pub use selection::{apply_selection_overlay, SelectionState};
pub use state::{reducer, Action, AppState, SealedPrefixFlushPolicy};
pub use streaming::{StreamingContentBlock, StreamingOverlay, StreamingThinking, StreamingToolUse};
pub use transcript::TranscriptStore;
pub use user_text::{
    detect_user_text_kind, extract_tag, UserTextKind, INTERRUPT_MESSAGE,
    INTERRUPT_MESSAGE_FOR_TOOL_USE, NO_CONTENT_MESSAGE,
};

#[cfg(test)]
mod compatibility {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Every `Cargo.toml` in the tree.
    fn manifests(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for group in [root.join("crates"), root.join("crates").join("plugins")] {
            let Ok(entries) = fs::read_dir(&group) else {
                continue;
            };
            for entry in entries.flatten() {
                let manifest = entry.path().join("Cargo.toml");
                if manifest.is_file() {
                    found.push(manifest);
                }
            }
        }
        found
    }

    /// The constraint the merged-module classification rests on: this is the only
    /// crate in the tree that pulls in ratatui as a library, and
    /// `rebon-cli` is the only crate allowed to depend on it.
    ///
    /// It is what decides whether a pure-logic crate may be merged in
    /// here. A second dependant would silently hand ratatui to whatever
    /// added the edge — a feature plugin, say — and the merged-in modules
    /// (`layout`, and the ones that follow it) would become unreachable for
    /// that consumer without it. Replaces the
    /// `no_rebon_deps_in_cargo_toml` canaries that each merged-in crate
    /// carried while it was still its own crate.
    #[test]
    fn only_rebon_cli_may_depend_on_this_crate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/rebon-tui sits two levels below the repo root")
            .to_path_buf();
        let mut dependants = Vec::new();
        for manifest in manifests(&root) {
            if manifest.parent().map(|dir| dir.ends_with("rebon-tui")) == Some(true) {
                continue;
            }
            let text = fs::read_to_string(&manifest).expect("read a workspace manifest");
            let names_us = text.lines().any(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with('#') && trimmed.starts_with("rebon-tui")
            });
            if names_us {
                dependants.push(manifest);
            }
        }
        let expected = root.join("crates").join("rebon-cli").join("Cargo.toml");
        assert_eq!(
            dependants,
            vec![expected],
            "rebon-tui is the terminal crate; only rebon-cli may depend on it"
        );
    }
}
