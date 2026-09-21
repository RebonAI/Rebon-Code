//! App state + reducer for the transcript slice.
//!
//! ## Cancel-commit semantics
//!
//! Cancelling a turn promotes everything still in the overlay — text and
//! tool uses alike — into committed transcript rows, and only then clears
//! the overlay. That way pressing Esc or Ctrl+C never loses content the
//! user already saw while streaming.
//!
//! The cancel reducer does it in this order:
//!
//! 1. Materialize any full live shell history the overlay was still
//!    holding, and mark in-flight workflows as interrupted.
//! 2. Group the overlay's blocks into chunks — a visible text segment
//!    starts a chunk, and the tool calls that follow it join that same
//!    chunk, while whitespace-only text and thinking are dropped — and
//!    push one [`AssistantMessage`](crate::message::AssistantMessage) row
//!    per chunk, each holding its `AssistantContentBlock`s.
//! 3. Clear the overlay.
//!
//! The committed rows take a caller-supplied uuid and timestamp instead of
//! generating them here (chunks after the first get `<uuid>-part-<idx>`),
//! so the reducer stays a pure function of its inputs, which is what makes
//! the tests deterministic. This module also documents the branch directly
//! in the `Cancel` reducer tests.
//!

use std::collections::HashMap;

use rebon_render::{classify_tool_use, ClassifyOptions, MemoryPathPolicy, ToolClass};
use rebon_types::{
    ContentBlock, RegularContent, TextContent, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolKind,
};
use serde_json::Value;

use crate::message::{
    AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
    AssistantTextBlock, AssistantThinkingBlock, AssistantToolUseBlock, Message,
};
use crate::streaming::{
    StreamingContentBlock, StreamingOverlay, StreamingToolUse, WORKFLOW_INTERRUPTED_MESSAGE,
};
use crate::transcript::TranscriptStore;

/// Combined state — the committed transcript store + the parallel
/// streaming overlay. Observers read both; the renderer paints the
/// store first, then draws the overlay below in a visually-
/// separated slot.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub transcript: TranscriptStore,
    pub overlay: StreamingOverlay,
    /// Monotonic counter incremented on every successful
    /// `FlushSealedPrefix` so progressively-committed assistant rows
    /// get unique uuids without colliding with the `commit_uuid`
    /// supplied to FinalizeTurn / Cancel.
    pub flush_counter: u32,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealedPrefixFlushPolicy {
    /// Keep any trailing completed tool cluster in the overlay so later
    /// tool calls in the same turn can visually group with it.
    HoldBackTrailingToolCluster,
    /// Inline mode treats terminal scrollback as immutable: drain sealed
    /// content whose rendered representation is closed/stable, including
    /// terminal tool output.
    DrainClosedStable,
    /// Inline overflow escape hatch: drain closed/stable assistant output and
    /// the current trailing text chunk. The trailing text is not strictly
    /// sealed, but when the terminal-height-capped inline viewport is already
    /// overflowing, keeping it live means its top rows can never reach terminal
    /// scrollback. Later text deltas start a fresh overlay text block.
    DrainClosedStableAndLiveText,
    /// Inline live flushing policy: keep a trailing groupable tool chain live
    /// until a group-breaking boundary appears, and keep an unanchored closed
    /// thinking run live until the next visible tool or assistant text decides
    /// whether it remains consecutive reasoning.
    DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
    /// Drain closed/stable output and the WHOLE trailing text block, partial
    /// last line included.
    ///
    /// [`Self::DrainClosedStableAndLiveText`] holds that last line back because
    /// more deltas are coming for it. This policy is for the callers that know
    /// none are: something is about to be committed *after* the text, and a
    /// committed message renders under the live overlay, so whatever stays live
    /// would render below the message that follows it.
    DrainClosedStableAndSealLiveText,
}

impl Default for SealedPrefixFlushPolicy {
    fn default() -> Self {
        Self::HoldBackTrailingToolCluster
    }
}

impl SealedPrefixFlushPolicy {
    fn drains_closed_thinking(self) -> bool {
        matches!(
            self,
            Self::DrainClosedStable
                | Self::DrainClosedStableAndLiveText
                | Self::DrainClosedStableAndSealLiveText
                | Self::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
        )
    }

    fn drains_terminal_text(self) -> bool {
        matches!(self, Self::DrainClosedStable)
    }

    fn drains_live_text_tail(self) -> bool {
        matches!(
            self,
            Self::DrainClosedStableAndLiveText | Self::DrainClosedStableAndSealLiveText
        )
    }

    /// Whether the trailing text block drains whole, partial last line and all.
    fn seals_live_text_tail(self) -> bool {
        matches!(self, Self::DrainClosedStableAndSealLiveText)
    }

    fn trailing_groupable_tool_hold_start(
        self,
        blocks: &[StreamingContentBlock],
        prefix_end: usize,
    ) -> Option<usize> {
        match self {
            Self::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary => {
                trailing_groupable_tool_chain_start(blocks, prefix_end)
            }
            Self::HoldBackTrailingToolCluster => {
                trailing_groupable_tool_cluster(blocks, prefix_end)
                    .first()
                    .copied()
            }
            Self::DrainClosedStable
            | Self::DrainClosedStableAndLiveText
            | Self::DrainClosedStableAndSealLiveText => None,
        }
    }

    fn holds_trailing_tool_cluster_until_group_boundary(
        self,
        blocks: &[StreamingContentBlock],
        prefix_end: usize,
    ) -> bool {
        match self {
            Self::HoldBackTrailingToolCluster => true,
            Self::DrainClosedStable
            | Self::DrainClosedStableAndLiveText
            | Self::DrainClosedStableAndSealLiveText => false,
            Self::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary => {
                if self
                    .trailing_groupable_tool_hold_start(blocks, prefix_end)
                    .is_none()
                {
                    return false;
                }
                let live_tail = &blocks[prefix_end..];
                let live_tail_has_group_boundary = live_tail.iter().any(|b| match b {
                    StreamingContentBlock::Text(text) => !text.trim().is_empty(),
                    StreamingContentBlock::ToolUse(tool) => {
                        !streaming_tool_class(tool).is_collapsible()
                    }
                    StreamingContentBlock::Thinking(_) => false,
                });
                !live_tail_has_group_boundary
            }
        }
    }
}

fn streaming_tool_class(tool: &StreamingToolUse) -> ToolClass {
    let input = tool
        .raw_input
        .as_ref()
        .map(|map| {
            let object = map
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            Value::Object(object)
        })
        .unwrap_or(Value::Null);
    classify_tool_use(
        &tool.tool_name,
        &input,
        &MemoryPathPolicy::default(),
        ClassifyOptions::default(),
    )
}

fn trailing_groupable_tool_cluster(
    blocks: &[StreamingContentBlock],
    prefix_end: usize,
) -> Vec<usize> {
    let mut trailing = Vec::new();
    let mut idx = prefix_end.min(blocks.len());
    while idx > 0 {
        idx -= 1;
        match &blocks[idx] {
            StreamingContentBlock::ToolUse(tool) if streaming_tool_class(tool).is_collapsible() => {
                trailing.push(idx);
            }
            StreamingContentBlock::Thinking(_) => {}
            StreamingContentBlock::Text(text) if text.trim().is_empty() => {}
            _ => break,
        }
    }
    trailing.reverse();
    trailing
}

fn trailing_groupable_tool_chain_start(
    blocks: &[StreamingContentBlock],
    prefix_end: usize,
) -> Option<usize> {
    let mut start = None;
    let mut idx = prefix_end.min(blocks.len());
    while idx > 0 {
        idx -= 1;
        match &blocks[idx] {
            StreamingContentBlock::Text(text) if !text.trim().is_empty() => break,
            StreamingContentBlock::ToolUse(tool) if streaming_tool_class(tool).is_collapsible() => {
                start = Some(idx);
            }
            StreamingContentBlock::ToolUse(_) if start.is_none() => return None,
            StreamingContentBlock::ToolUse(_)
            | StreamingContentBlock::Thinking(_)
            | StreamingContentBlock::Text(_) => {}
        }
    }
    start
}

fn trailing_thinking_run_start(
    blocks: &[StreamingContentBlock],
    prefix_end: usize,
) -> Option<usize> {
    let mut start = None;
    let mut idx = prefix_end.min(blocks.len());
    while idx > 0 {
        idx -= 1;
        match &blocks[idx] {
            StreamingContentBlock::Thinking(_) => start = Some(idx),
            StreamingContentBlock::Text(text) if text.trim().is_empty() => {}
            StreamingContentBlock::Text(_) | StreamingContentBlock::ToolUse(_) => break,
        }
    }
    start
}

/// Reducer action. Narrowly scoped on purpose — this module
/// only needs the actions that let tests exercise the cancel-commit
/// ordering, tool overlay updates, and the streaming text append
/// path. Future runtime integration will extend this as the engine bridge grows.
#[derive(Debug, Clone)]
pub enum Action {
    /// Append a fully-formed message to the transcript store. This
    /// is what the engine bridge will call once it has a typed
    /// `Message` ready.
    Commit(Message),
    /// Set the streaming text, replacing whatever was there.
    SetStreamingText(String),
    /// Append a delta to the streaming text, accumulating onto the text
    /// already in the overlay.
    AppendStreamingText(String),
    /// Upsert a streaming tool-use entry keyed by ACP `toolCallId`.
    StartToolUse {
        call_id: String,
        tool_name: String,
        kind: ToolKind,
        initial_status: ToolCallStatus,
        initial_title: Option<String>,
        raw_input: Option<HashMap<String, Value>>,
        content: Option<Vec<ToolCallContent>>,
        locations: Option<Vec<ToolCallLocation>>,
        raw_output: Option<HashMap<String, Value>>,
    },
    /// Patch an existing streaming tool-use entry in place.
    UpdateToolUse {
        call_id: String,
        status: Option<ToolCallStatus>,
        title: Option<String>,
        content: Option<Vec<ToolCallContent>>,
        locations: Option<Vec<ToolCallLocation>>,
        raw_output: Option<HashMap<String, Value>>,
    },
    /// Cancel the current turn. Promotes all visible overlay content
    /// (text + tool uses) to a committed transcript row, then clears
    /// the overlay. This preserves everything the user saw during
    /// streaming so ESC / Ctrl+C doesn't lose content.
    /// The reducer needs a caller-supplied uuid+timestamp for the
    /// committed rows so tests stay deterministic.
    Cancel {
        commit_uuid: String,
        commit_timestamp: String,
    },
    /// Normal turn-complete finalizer for the successful end of
    /// a streamed turn: promote visible partial assistant text to the
    /// transcript and clear the overlay.
    FinalizeTurn {
        commit_uuid: String,
        commit_timestamp: String,
    },
    /// Remove tail transcript rows back to `len`. This is used for
    /// safe local undo of a just-submitted user turn before any
    /// assistant-visible output has started.
    TruncateTranscript { len: usize },
    /// Clear the overlay without committing. Used by paths where the
    /// final assistant message has already been committed through its
    /// own `Commit(_)` action.
    ClearOverlay,
    /// Append a delta to the streaming thinking block. Creates the
    /// `StreamingThinking` if it doesn't exist yet.
    AppendStreamingThinking(String),
    /// Mark the streaming thinking block as finished (no longer
    /// actively streaming).
    EndStreamingThinking,
    /// Progressive commit: drain the longest sealed prefix of the
    /// streaming overlay into the transcript as one or more assistant
    /// messages, and leave the in-flight tail in the overlay.
    ///
    /// A block is "sealed" iff it can no longer receive deltas:
    /// - `Text`: not the last overlay block
    /// - `ToolUse`: status is terminal (`Completed` / `Failed`)
    /// - `Thinking`: the *group* it anchors is sealed (i.e. the
    ///   following Text/ToolUse anchor is sealed)
    ///
    /// This keeps the overlay bounded — typically a single in-flight
    /// block — so the renderer never needs to drop oldest segments
    /// to fit the viewport. `policy` lets inline mode drain terminal
    /// tool clusters immediately while screen mode keeps the historical
    /// grouping hold-back.
    FlushSealedPrefix {
        commit_timestamp: String,
        policy: SealedPrefixFlushPolicy,
    },
}

/// Commit visible overlay content into committed assistant rows while
/// preserving the streaming-time boundaries that drive collapse.
///
/// The streaming renderer groups `overlay.blocks`, not a synthetic
/// "whole-turn" assistant message. If finalize flattens everything into
/// one assistant row, any trailing text after a tool call becomes part of
/// that same message and the committed transcript loses the collapse
/// eligibility it had while streaming. To keep read/search groups folded
/// after commit, we persist the overlay as multiple assistant messages:
///
/// - each visible text segment becomes its own committed message;
/// - each tool call becomes its own committed message, carrying any
///   leading thinking that was part of that tool step;
/// - thinking that would otherwise strand on its own is attached to the
///   next visible text segment when there is one, or to the preceding
///   committed message at end-of-turn.
fn assistant_text_block(text: &str) -> Option<AssistantContentBlock> {
    if text.trim().is_empty() {
        None
    } else {
        Some(AssistantContentBlock::Text(AssistantTextBlock {
            text: text.to_string(),
        }))
    }
}

fn assistant_thinking_block(thinking: &str) -> Option<AssistantContentBlock> {
    if thinking.trim().is_empty() {
        None
    } else {
        Some(AssistantContentBlock::Thinking(AssistantThinkingBlock {
            thinking: thinking.to_string(),
            signature: None,
        }))
    }
}

fn assistant_tool_use_block(tool: &StreamingToolUse) -> AssistantContentBlock {
    let input = tool
        .raw_input
        .as_ref()
        .and_then(|m| serde_json::to_value(m).ok())
        .unwrap_or(Value::Object(Default::default()));
    let raw_output = tool
        .raw_output
        .as_ref()
        .and_then(|m| serde_json::to_value(m).ok());
    AssistantContentBlock::ToolUse(AssistantToolUseBlock {
        id: tool.call_id.clone(),
        name: tool.tool_name.clone(),
        input,
        tool_call_content: tool.content.clone(),
        raw_output,
        title: tool.title.clone(),
        locations: tool.locations.clone(),
        status: Some(tool.status),
    })
}

/// Group a slice of streaming overlay blocks into the assistant-row
/// chunks that get pushed to the transcript. Each Text or ToolUse
/// closes a chunk; leading Thinking blocks attach to the next
/// non-Thinking anchor. Trailing Thinking that has no anchor
/// attaches to the last chunk if any, otherwise becomes its own.
fn group_blocks_into_chunks(blocks: &[StreamingContentBlock]) -> Vec<Vec<AssistantContentBlock>> {
    let mut chunks: Vec<Vec<AssistantContentBlock>> = Vec::new();
    let mut pending_non_tool: Vec<AssistantContentBlock> = Vec::new();

    for block in blocks {
        match block {
            StreamingContentBlock::Text(text) => {
                let Some(text_block) = assistant_text_block(text) else {
                    continue;
                };
                if pending_non_tool.is_empty() {
                    chunks.push(vec![text_block]);
                } else {
                    pending_non_tool.push(text_block);
                    chunks.push(std::mem::take(&mut pending_non_tool));
                }
            }
            StreamingContentBlock::Thinking(thinking) => {
                if let Some(thinking_block) = assistant_thinking_block(&thinking.thinking) {
                    pending_non_tool.push(thinking_block);
                }
            }
            StreamingContentBlock::ToolUse(tool) => {
                let mut content = std::mem::take(&mut pending_non_tool);
                content.push(assistant_tool_use_block(tool));
                chunks.push(content);
            }
        }
    }

    if !pending_non_tool.is_empty() {
        if let Some(last) = chunks.last_mut() {
            last.append(&mut pending_non_tool);
        } else {
            chunks.push(pending_non_tool);
        }
    }

    chunks
}

/// Determine how many overlay blocks form the longest sealed prefix.
/// "Sealed" means the block can no longer receive deltas; see the
/// docstring on `Action::FlushSealedPrefix`.
///
/// Walks forward; each Text/ToolUse anchor either advances the prefix
/// (if sealed) or breaks. Thinking blocks count toward the prefix
/// only when followed by a sealed anchor, which falls out naturally:
/// we update `end` on each sealed anchor's index + 1, so any leading
/// Thinking already in `[0..end)` stays included.
///
/// **Trailing tool_use cluster is held back.** If the prefix would
/// end on a tool_use, we pull `end` back to just past the last Text
/// block in the prefix. This keeps consecutive completed tool_uses in
/// the overlay so the streaming segment builder can group them as a
/// `Collapsed` run alongside any later tool_uses that arrive in the
/// same turn — without that hold-back, terminal tool_uses would flush
/// to the committed transcript one-by-one and the collapsed group
/// would visually snap into place only after the final tool completed.
fn sealed_prefix_block_count(
    blocks: &[StreamingContentBlock],
    policy: SealedPrefixFlushPolicy,
) -> usize {
    let n = blocks.len();
    if n == 0 {
        return 0;
    }
    let mut end = 0;
    for (i, block) in blocks.iter().enumerate() {
        let is_last = i == n - 1;
        match block {
            StreamingContentBlock::Thinking(t) => {
                if t.is_streaming {
                    break;
                }
                if policy.drains_closed_thinking() {
                    end = i + 1;
                }
            }
            StreamingContentBlock::Text(text) => {
                if is_last {
                    if policy.drains_live_text_tail() && !text.trim().is_empty() {
                        end = i + 1;
                    } else if policy.drains_terminal_text()
                        && (i > 0
                            && matches!(
                                &blocks[i - 1],
                                StreamingContentBlock::Thinking(t) if !t.is_streaming
                            )
                            || blocks[..i].iter().any(|block| {
                                matches!(block, StreamingContentBlock::ToolUse(tool) if matches!(tool.status, ToolCallStatus::Completed | ToolCallStatus::Failed))
                            }))
                    {
                        end = i + 1;
                    }
                    break;
                }
                end = i + 1;
            }
            StreamingContentBlock::ToolUse(t) => {
                let terminal =
                    matches!(t.status, ToolCallStatus::Completed | ToolCallStatus::Failed);
                if !terminal {
                    break;
                }
                end = i + 1;
            }
        }
    }

    // Any policy that preserves live grouping must hold back an open trailing
    // groupable tool cluster. Inline mode keeps read/search clusters live while
    // the tail can still join the same collapse group; a later group-breaking
    // boundary lets the completed cluster drain so immutable scrollback does not
    // miss it.
    if end > 0 {
        let mut idx = end;
        while idx > 0 {
            idx -= 1;
            match &blocks[idx] {
                StreamingContentBlock::Thinking(_) => {
                    if matches!(policy, SealedPrefixFlushPolicy::DrainClosedStable) {
                        break;
                    }
                    continue;
                }
                StreamingContentBlock::Text(text) => {
                    if policy.drains_closed_thinking() && text.trim().is_empty() {
                        continue;
                    }
                    break;
                }
                StreamingContentBlock::ToolUse(_tool) => {
                    let hold_start = policy.trailing_groupable_tool_hold_start(blocks, end);
                    if let Some(hold_start) = hold_start {
                        if policy.holds_trailing_tool_cluster_until_group_boundary(blocks, end) {
                            let should_hold = match policy {
                                SealedPrefixFlushPolicy::DrainClosedStable
                                | SealedPrefixFlushPolicy::DrainClosedStableAndLiveText
                                | SealedPrefixFlushPolicy::DrainClosedStableAndSealLiveText => false,
                                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary => true,
                                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster => {
                                    let previous_meaningful = blocks[..idx]
                                        .iter()
                                        .rev()
                                        .find(|block| !matches!(block, StreamingContentBlock::Thinking(_)));
                                    let has_text_boundary_on_both_sides = matches!(
                                        previous_meaningful,
                                        Some(StreamingContentBlock::Text(text)) if !text.trim().is_empty()
                                    ) && idx + 1 == blocks.len()
                                        && !blocks[..idx]
                                            .iter()
                                            .any(|block| matches!(block, StreamingContentBlock::Thinking(_)));
                                    !has_text_boundary_on_both_sides
                                }
                            };
                            if should_hold {
                                end = hold_start;
                                // Leading thinking (and blank text) directly
                                // before the held cluster attaches to that
                                // cluster's first anchor at commit time
                                // (`group_blocks_into_chunks` folds leading
                                // thinking into the next anchor's chunk).
                                // Draining it alone would commit a
                                // thinking-only assistant row and detach the
                                // reasoning from its tool step, so hold those
                                // blocks back with the cluster.
                                while end > 0 {
                                    match &blocks[end - 1] {
                                        StreamingContentBlock::Thinking(_) => end -= 1,
                                        StreamingContentBlock::Text(text)
                                            if text.trim().is_empty() =>
                                        {
                                            end -= 1
                                        }
                                        _ => break,
                                    }
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
    if matches!(
        policy,
        SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
    ) {
        let tail_has_visible_boundary = blocks[end..].iter().any(|block| {
            matches!(block, StreamingContentBlock::ToolUse(_))
                || matches!(block, StreamingContentBlock::Text(text) if !text.trim().is_empty())
        });
        // Keep only the trailing uninterrupted thinking run mutable. A visible
        // tool is a reasoning boundary, so thinking before it can safely enter
        // immutable inline scrollback; hidden tools never reach this overlay.
        if !tail_has_visible_boundary {
            if let Some(thinking_start) = trailing_thinking_run_start(blocks, end) {
                end = thinking_start;
            }
        }
    }
    end
}

fn push_assistant_chunks(
    state: &mut AppState,
    chunks: Vec<Vec<AssistantContentBlock>>,
    timestamp: &str,
    first_chunk_is_continuation: bool,
    uuid_for_idx: impl Fn(usize) -> String,
) {
    for (idx, content) in chunks.into_iter().enumerate() {
        if content.is_empty() {
            continue;
        }
        state.transcript.push(Message::Assistant(AssistantMessage {
            uuid: uuid_for_idx(idx),
            timestamp: timestamp.to_string(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content,
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: (idx == 0 && first_chunk_is_continuation).then_some(true),
        }));
    }
}

fn workflow_interrupted_content() -> ToolCallContent {
    ToolCallContent::Content(RegularContent {
        content: ContentBlock::Text(TextContent {
            text: WORKFLOW_INTERRUPTED_MESSAGE.to_string(),
            annotations: None,
        }),
    })
}

fn mark_interrupted_workflows(blocks: &mut [StreamingContentBlock]) {
    for block in blocks {
        let StreamingContentBlock::ToolUse(tool) = block else {
            continue;
        };
        if !crate::streaming::is_workflow_tool_use(&tool.tool_name, tool.raw_input.as_ref())
            || !matches!(
                tool.status,
                ToolCallStatus::Pending | ToolCallStatus::InProgress
            )
        {
            continue;
        }
        tool.status = ToolCallStatus::Failed;
        match &mut tool.content {
            Some(content) => content.push(workflow_interrupted_content()),
            None => tool.content = Some(vec![workflow_interrupted_content()]),
        }
    }
}

fn commit_streaming_content_and_clear(
    state: &mut AppState,
    commit_uuid: String,
    commit_timestamp: String,
) {
    state.overlay.materialize_full_live_shell_content();
    mark_interrupted_workflows(&mut state.overlay.blocks);
    let first_chunk_is_continuation = first_block_is_committable_continuation(&state.overlay);
    let chunks = group_blocks_into_chunks(&state.overlay.blocks);
    push_assistant_chunks(
        state,
        chunks,
        &commit_timestamp,
        first_chunk_is_continuation,
        |idx| {
            if idx == 0 {
                commit_uuid.clone()
            } else {
                format!("{commit_uuid}-part-{idx}")
            }
        },
    );
    state.overlay.clear();
}

/// Whether the overlay's first block is the live remainder of an
/// overflow-split text block AND will produce a committed row (i.e. it
/// is non-empty text, so `group_blocks_into_chunks` keeps it as the
/// first chunk). An empty remainder is dropped by chunking, and the row
/// that then leads the commit is not a text continuation.
fn first_block_is_committable_continuation(overlay: &StreamingOverlay) -> bool {
    overlay.first_text_is_continuation
        && matches!(
            overlay.blocks.first(),
            Some(StreamingContentBlock::Text(text)) if !text.trim().is_empty()
        )
}

fn flush_sealed_prefix(
    state: &mut AppState,
    commit_timestamp: String,
    policy: SealedPrefixFlushPolicy,
) {
    let blocks = &state.overlay.blocks;
    let mut prefix_end = sealed_prefix_block_count(blocks, policy);
    if !blocks.is_empty() {
        let block_summary: Vec<&str> = blocks
            .iter()
            .map(|b| match b {
                StreamingContentBlock::Text(t) => {
                    if t.trim().is_empty() {
                        "Text(empty)"
                    } else {
                        "Text"
                    }
                }
                StreamingContentBlock::ToolUse(t) => match t.status {
                    ToolCallStatus::Pending => "Tool(Pending)",
                    ToolCallStatus::InProgress => "Tool(InProgress)",
                    ToolCallStatus::Completed => "Tool(Completed)",
                    ToolCallStatus::Failed => "Tool(Failed)",
                },
                StreamingContentBlock::Thinking(t) => {
                    if t.is_streaming {
                        "Think(streaming)"
                    } else {
                        "Think(closed)"
                    }
                }
            })
            .collect();
        tracing::trace!(
            target: "inline_flush",
            ?policy,
            prefix_end,
            overlay_len = blocks.len(),
            ?block_summary,
            "flush_sealed_prefix"
        );
    }
    if prefix_end == 0 {
        return;
    }
    // A trailing Text block included via `drains_live_text_tail` is still
    // receiving deltas. Draining all of it would cut the message at an
    // arbitrary byte (mid-word) and the continuation would re-render as a
    // separate bullet row. Split at the last newline instead: complete
    // lines commit, the partial trailing line stays live and keeps
    // accumulating deltas. With no complete line yet, hold the whole block.
    let mut live_text_remainder: Option<String> = None;
    if policy.drains_live_text_tail()
        && !policy.seals_live_text_tail()
        && prefix_end == state.overlay.blocks.len()
    {
        if let Some(StreamingContentBlock::Text(text)) = state.overlay.blocks.last() {
            match text.rfind('\n') {
                Some(cut) if !text[..cut].trim().is_empty() => {
                    live_text_remainder = Some(text[cut + 1..].to_string());
                }
                _ => prefix_end -= 1,
            }
        }
    }
    if prefix_end == 0 {
        return;
    }
    let first_chunk_is_continuation = first_block_is_committable_continuation(&state.overlay);
    let mut drained: Vec<StreamingContentBlock> =
        state.overlay.blocks.drain(0..prefix_end).collect();
    // Block 0 was just consumed; re-armed below if this flush splits the
    // live text again and leaves a fresh remainder at block 0.
    state.overlay.first_text_is_continuation = false;
    if let Some(mut remainder) = live_text_remainder {
        if let Some(StreamingContentBlock::Text(text)) = drained.last_mut() {
            text.truncate(text.len() - remainder.len() - 1);
            // The committed half and the live remainder re-render as
            // independent markdown documents, so a cut inside an
            // unclosed code fence would leave the remainder's lines
            // unstyled and let the message's original closing fence
            // *open* a fence there, inverting code/prose styling for
            // the rest of the message. Close the fence on the committed
            // side and re-open it on the live side; fence lines are
            // consumed by the parser, so nothing extra is displayed.
            if let Some(fence) = rebon_render::open_fence_at_end(text) {
                text.push('\n');
                text.push_str(&fence.closing_line());
                remainder.insert(0, '\n');
                remainder.insert_str(0, &fence.opening_line());
            }
        }
        state
            .overlay
            .blocks
            .insert(0, StreamingContentBlock::Text(remainder));
        state.overlay.first_text_is_continuation = true;
    }
    let chunks = group_blocks_into_chunks(&drained);
    if chunks.iter().all(|c| c.is_empty()) {
        return;
    }
    let counter = state.flush_counter;
    state.flush_counter = state.flush_counter.wrapping_add(1);
    push_assistant_chunks(
        state,
        chunks,
        &commit_timestamp,
        first_chunk_is_continuation,
        |idx| format!("partial-{counter}-{idx}"),
    );
}

/// Apply an action to the state in place.
pub fn reducer(state: &mut AppState, action: Action) {
    match action {
        Action::Commit(msg) => state.transcript.push(msg),
        Action::SetStreamingText(text) => {
            // A visible text block means any open reasoning stream has
            // ended, even when the engine's ThinkingEnd event was lost
            // (message ended on a thinking block, stream retry, provider
            // quirk). A thinking block stuck `is_streaming` stalls the
            // inline sealed-prefix flush at that block for the rest of
            // the turn, stranding everything after it in the live
            // overlay. Normal streams close thinking via ThinkingEnd
            // before this fires, so this is a no-op there.
            state.overlay.close_all_streaming_thinking();
            state.overlay.set_streaming_text(text)
        }
        Action::AppendStreamingText(delta) => {
            state.overlay.close_all_streaming_thinking();
            state.overlay.append_streaming_text(&delta)
        }
        Action::StartToolUse {
            call_id,
            tool_name,
            kind,
            initial_status,
            initial_title,
            raw_input,
            content,
            locations,
            raw_output,
        } => {
            state.overlay.close_all_streaming_thinking();
            state.overlay.upsert_streaming_tool_use(StreamingToolUse {
                call_id,
                tool_name,
                kind,
                status: initial_status,
                title: initial_title,
                content,
                locations,
                raw_input,
                raw_output,
            })
        }
        Action::UpdateToolUse {
            call_id,
            status,
            title,
            content,
            locations,
            raw_output,
        } => {
            let updated = state
                .overlay
                .update_streaming_tool_use(&call_id, status, title, content, locations, raw_output);
            if !updated {
                // The update was dropped. If it carried the terminal
                // status, the block (wherever it now lives) stays
                // non-terminal and blocks the inline sealed-prefix
                // flush behind it.
                tracing::warn!(
                    %call_id,
                    ?status,
                    "UpdateToolUse matched no overlay block; update dropped"
                );
            }
        }
        Action::Cancel {
            commit_uuid,
            commit_timestamp,
        } => commit_streaming_content_and_clear(state, commit_uuid, commit_timestamp),
        Action::FinalizeTurn {
            commit_uuid,
            commit_timestamp,
        } => commit_streaming_content_and_clear(state, commit_uuid, commit_timestamp),
        Action::TruncateTranscript { len } => {
            state.transcript.truncate(len);
        }
        Action::ClearOverlay => state.overlay.clear(),
        Action::AppendStreamingThinking(delta) => {
            state.overlay.append_streaming_thinking(&delta);
        }
        Action::EndStreamingThinking => {
            state.overlay.end_streaming_thinking();
        }
        Action::FlushSealedPrefix {
            commit_timestamp,
            policy,
        } => {
            flush_sealed_prefix(state, commit_timestamp, policy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        UserContentBlock, UserMessage, UserMessageInner, UserRole, UserTextBlock,
    };
    use serde_json::json;

    fn start_tool_use(call_id: &str) -> Action {
        Action::StartToolUse {
            call_id: call_id.into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            initial_status: ToolCallStatus::Pending,
            initial_title: Some("Read Cargo.toml".into()),
            raw_input: Some(HashMap::from([(
                "path".into(),
                Value::String("Cargo.toml".into()),
            )])),
            content: None,
            locations: None,
            raw_output: None,
        }
    }

    fn user(uuid: &str, text: &str) -> Message {
        Message::User(UserMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: UserMessageInner {
                role: UserRole::User,
                content: vec![UserContentBlock::Text(UserTextBlock { text: text.into() })],
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids: None,
            plan_content: None,
        })
    }

    #[test]
    fn commit_appends_to_transcript() {
        let mut s = AppState::new();
        reducer(&mut s, Action::Commit(user("u1", "hi")));
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript.rows()[0].uuid(), Some("u1"));
    }

    #[test]
    fn truncate_transcript_action_drops_tail_and_preserves_overlay() {
        let mut s = AppState::new();
        reducer(&mut s, Action::Commit(user("u1", "first")));
        reducer(&mut s, Action::Commit(user("u2", "second")));
        reducer(&mut s, Action::SetStreamingText("partial".into()));
        let before = s.transcript.revision();

        reducer(&mut s, Action::TruncateTranscript { len: 1 });

        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript.rows()[0].uuid(), Some("u1"));
        assert!(s.transcript.get("u2").is_none());
        assert!(s.transcript.revision() > before);
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("partial")
        );
    }

    #[test]
    fn overflow_flush_policy_drains_trailing_live_text_but_stable_policy_does_not() {
        let blocks = vec![text_block("long streaming answer")];

        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            0
        );
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableAndLiveText
            ),
            1
        );
    }

    #[test]
    fn overflow_flush_policy_commits_live_text_up_to_last_newline() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::AppendStreamingText("first line\nsecond line\npartial tail".into()),
        );

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::Text(t) if t.text == "first line\nsecond line"
            )),
            other => panic!("expected assistant text row, got {other:?}"),
        }
        // The partial trailing line stays live and keeps accumulating deltas.
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("partial tail")
        );
        reducer(&mut s, Action::AppendStreamingText(" continues".into()));
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("partial tail continues")
        );
    }

    #[test]
    fn overflow_flush_closes_and_reopens_code_fence_across_the_cut() {
        // Cutting a live text block inside an unclosed ```-fence must
        // not invert code/prose styling: the committed half gets a
        // closing fence, the live remainder re-opens an identical one,
        // and the message's real closer later matches the re-opened
        // fence instead of opening a new one.
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::AppendStreamingText(
                "验证已过：\n\n```text\ncargo fmt --all --check\ncargo test -p rebon-api\npartial"
                    .into(),
            ),
        );

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::Text(t)
                    if t.text
                        == "验证已过：\n\n```text\ncargo fmt --all --check\ncargo test -p rebon-api\n```"
            )),
            other => panic!("expected assistant text row, got {other:?}"),
        }
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("```text\npartial")
        );

        // The message's original closing fence now closes the re-opened
        // fence, so the continuation ends outside any code block.
        reducer(
            &mut s,
            Action::AppendStreamingText(" tail\n```\n\nprose".into()),
        );
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("```text\npartial tail\n```\n\nprose")
        );
    }

    #[test]
    fn overflow_split_marks_next_commit_as_stream_continuation() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::AppendStreamingText("first line\npartial".into()),
        );
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );

        // The committed top half is a normal row; the live remainder is
        // marked so both the streaming renderer and the eventual commit
        // join it to that half.
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => assert_eq!(a.is_stream_continuation, None),
            other => panic!("expected assistant text row, got {other:?}"),
        }
        assert!(s.overlay.first_text_is_continuation);

        reducer(&mut s, Action::AppendStreamingText(" tail".into()));
        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "final".into(),
                commit_timestamp: "t2".into(),
            },
        );

        assert!(!s.overlay.first_text_is_continuation);
        assert_eq!(s.transcript.len(), 2);
        match &s.transcript.rows()[1] {
            Message::Assistant(a) => {
                assert_eq!(a.is_stream_continuation, Some(true));
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::Text(t) if t.text == "partial tail"
                ));
            }
            other => panic!("expected continuation assistant row, got {other:?}"),
        }
    }

    #[test]
    fn repeated_overflow_splits_chain_continuation_rows() {
        // A second overflow split: the remainder committed by the second
        // flush is itself a continuation, and the flag re-arms for the
        // fresh remainder it leaves live.
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingText("one\ntwo".into()));
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );
        reducer(&mut s, Action::AppendStreamingText(" more\nthree".into()));
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );

        assert_eq!(s.transcript.len(), 2);
        match &s.transcript.rows()[1] {
            Message::Assistant(a) => {
                assert_eq!(a.is_stream_continuation, Some(true));
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::Text(t) if t.text == "two more"
                ));
            }
            other => panic!("expected continuation assistant row, got {other:?}"),
        }
        assert!(
            s.overlay.first_text_is_continuation,
            "the fresh remainder left live by the second split re-arms the flag"
        );
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("three")
        );
    }

    #[test]
    fn empty_split_remainder_does_not_mark_a_continuation_row() {
        // Text ending exactly on a newline leaves an empty live remainder.
        // Chunking drops it at finalize, so nothing gets marked and no
        // empty continuation row is committed.
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::AppendStreamingText("complete line\n".into()),
        );
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );
        assert_eq!(s.transcript.len(), 1);
        assert!(s.overlay.first_text_is_continuation);

        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "final".into(),
                commit_timestamp: "t2".into(),
            },
        );
        assert_eq!(
            s.transcript.len(),
            1,
            "an empty remainder must not commit an extra row"
        );
        assert!(!s.overlay.first_text_is_continuation);
    }

    #[test]
    fn overflow_flush_does_not_inject_fences_when_cut_is_outside_a_fence() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::AppendStreamingText("```sh\nls\n```\nafter the block\npartial".into()),
        );

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::Text(t)
                    if t.text == "```sh\nls\n```\nafter the block"
            )),
            other => panic!("expected assistant text row, got {other:?}"),
        }
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("partial")
        );
    }

    #[test]
    fn overflow_flush_policy_holds_live_text_without_a_complete_line() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::SetStreamingText("long streaming answer".into()),
        );

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );

        assert!(
            s.transcript.is_empty(),
            "no complete line yet — draining would cut the message mid-word"
        );
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("long streaming answer")
        );
    }

    #[test]
    fn overflow_flush_policy_drains_sealed_blocks_but_holds_lineless_live_text() {
        let mut s = AppState::new();
        reducer(&mut s, start_tool_use("toolu_1"));
        reducer(
            &mut s,
            Action::UpdateToolUse {
                call_id: "toolu_1".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        reducer(&mut s, Action::AppendStreamingText("no newline yet".into()));

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );

        assert_eq!(
            s.transcript.len(),
            1,
            "the completed tool must still drain even though the live text is held"
        );
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("no newline yet")
        );
        assert_eq!(s.overlay.tool_use_count(), 0);
    }

    #[test]
    fn set_streaming_text_leaves_transcript_unchanged() {
        let mut s = AppState::new();
        reducer(&mut s, Action::SetStreamingText("partial".into()));
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("partial")
        );
        assert!(
            s.transcript.is_empty(),
            "streaming must not mutate transcript"
        );
    }

    #[test]
    fn append_streaming_text_accumulates() {
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingText("foo ".into()));
        reducer(&mut s, Action::AppendStreamingText("bar".into()));
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("foo bar")
        );
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn start_tool_use_appends_to_streaming_overlay() {
        let mut s = AppState::new();
        reducer(&mut s, start_tool_use("toolu_1"));

        assert_eq!(s.overlay.tool_use_count(), 1);
        let tool = s.overlay.find_tool_use("toolu_1").unwrap();
        assert_eq!(tool.call_id, "toolu_1");
        assert_eq!(tool.tool_name, "Read");
        assert_eq!(tool.status, ToolCallStatus::Pending);
        assert_eq!(tool.title.as_deref(), Some("Read Cargo.toml"));
    }

    #[test]
    fn append_streaming_text_closes_leaked_streaming_thinking() {
        // The engine closes thinking via ThinkingEnd on the next block
        // start, but that event can be lost (message ended on a thinking
        // block, stream retry). Visible text arriving means reasoning is
        // over regardless — the reducer must close the leak itself.
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingThinking("orphaned".into()));
        reducer(&mut s, Action::AppendStreamingText("answer".into()));
        assert!(matches!(
            &s.overlay.blocks[0],
            StreamingContentBlock::Thinking(t) if !t.is_streaming
        ));
    }

    #[test]
    fn set_streaming_text_closes_leaked_streaming_thinking() {
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingThinking("orphaned".into()));
        reducer(&mut s, Action::SetStreamingText("answer".into()));
        assert!(matches!(
            &s.overlay.blocks[0],
            StreamingContentBlock::Thinking(t) if !t.is_streaming
        ));
    }

    #[test]
    fn start_tool_use_closes_leaked_streaming_thinking() {
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingThinking("orphaned".into()));
        reducer(&mut s, start_tool_use("toolu_1"));
        assert!(matches!(
            &s.overlay.blocks[0],
            StreamingContentBlock::Thinking(t) if !t.is_streaming
        ));
    }

    #[test]
    fn sealed_prefix_recovers_when_lost_thinking_end_is_defensively_closed() {
        // Regression for the inline "everything stays live until end of
        // turn" stall: a thinking block whose ThinkingEnd was lost kept
        // `is_streaming = true`, the sealed-prefix forward walk broke on
        // it, and every later completed tool/text block was stranded in
        // the overlay — the overflow force-drain uses the same walk, so
        // it drained nothing either, and the whole turn flushed to
        // scrollback only at FinalizeTurn.
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingThinking("orphaned".into()));
        reducer(&mut s, start_tool_use("toolu_1"));
        reducer(
            &mut s,
            Action::UpdateToolUse {
                call_id: "toolu_1".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        reducer(&mut s, Action::AppendStreamingText("done".into()));

        let prefix = sealed_prefix_block_count(
            &s.overlay.blocks,
            SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
        );
        assert_eq!(
            prefix, 3,
            "closed thinking + completed tool + live text must all seal \
             once the leaked thinking block is defensively closed"
        );
    }

    #[test]
    fn update_tool_use_for_unknown_call_id_is_dropped_without_panic() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::UpdateToolUse {
                call_id: "toolu_never_started".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        assert!(s.overlay.is_empty());
    }

    #[test]
    fn update_tool_use_transitions_status() {
        let mut s = AppState::new();
        reducer(&mut s, start_tool_use("toolu_1"));
        reducer(
            &mut s,
            Action::UpdateToolUse {
                call_id: "toolu_1".into(),
                status: Some(ToolCallStatus::Completed),
                title: Some("Read lockfile".into()),
                content: Some(vec![ToolCallContent::Content(
                    rebon_types::RegularContent {
                        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                            text: "done".into(),
                            annotations: None,
                        }),
                    },
                )]),
                locations: Some(vec![ToolCallLocation {
                    path: "Cargo.lock".into(),
                    line: Some(1),
                }]),
                raw_output: Some(HashMap::from([("bytes".into(), json!(42))])),
            },
        );

        let tool = s.overlay.find_tool_use("toolu_1").unwrap();
        assert_eq!(tool.status, ToolCallStatus::Completed);
        assert_eq!(tool.title.as_deref(), Some("Read lockfile"));
        assert!(tool.content.is_some());
        assert_eq!(tool.locations.as_ref().unwrap()[0].path, "Cargo.lock");
        assert_eq!(tool.raw_output.as_ref().unwrap()["bytes"], json!(42));
        assert_eq!(
            tool.raw_input.as_ref().unwrap()["path"],
            json!("Cargo.toml")
        );
    }

    /// The load-bearing alignment test: cancel-commit builds a real
    /// `AssistantMessage`-shaped row whose content array is the shape the
    /// transcript expects. The committed row must be visible in the
    /// transcript and the overlay must be fully cleared.
    #[test]
    fn cancel_commits_visible_partial_as_assistant_message() {
        let mut s = AppState::new();
        reducer(&mut s, Action::Commit(user("u1", "question?")));
        reducer(&mut s, Action::SetStreamingText("partial reply".into()));
        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "a-cancel".into(),
                commit_timestamp: "2026-04-07T12:00:00.000Z".into(),
            },
        );

        assert_eq!(s.transcript.len(), 2);
        // Committed partial must be a Message::Assistant with role
        // = assistant and content = [{type:'text', text:'partial reply'}].
        match &s.transcript.rows()[1] {
            Message::Assistant(a) => {
                assert_eq!(a.uuid, "a-cancel");
                assert_eq!(a.timestamp, "2026-04-07T12:00:00.000Z");
                assert_eq!(a.message.content.len(), 1);
                match &a.message.content[0] {
                    AssistantContentBlock::Text(t) => assert_eq!(t.text, "partial reply"),
                    other => panic!("expected Text content block, got {other:?}"),
                }
                assert_eq!(a.is_api_error_message, None);
                assert_eq!(a.advisor_model, None);
            }
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
        // Overlay fully cleared.
        assert!(s.overlay.is_empty());
    }

    /// Committed partial serializes back to the exact JSON shape of a
    /// committed assistant row. This is the wire-shape guarantee.
    #[test]
    fn cancel_committed_row_serializes_to_expected_shape() {
        let mut s = AppState::new();
        reducer(&mut s, Action::SetStreamingText("hello world".into()));
        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "a-1".into(),
                commit_timestamp: "2026-04-07T00:00:00.000Z".into(),
            },
        );
        let row = &s.transcript.rows()[0];
        let value = serde_json::to_value(row).unwrap();
        // Top-level shape of a committed assistant row (minus the fields
        // this crate does not model yet).
        assert_eq!(value["type"], "assistant");
        assert_eq!(value["uuid"], "a-1");
        assert_eq!(value["timestamp"], "2026-04-07T00:00:00.000Z");
        assert_eq!(value["message"]["role"], "assistant");
        assert_eq!(value["message"]["content"][0]["type"], "text");
        assert_eq!(value["message"]["content"][0]["text"], "hello world");
    }

    #[test]
    fn cancel_drops_whitespace_only_partial() {
        let mut s = AppState::new();
        reducer(&mut s, Action::SetStreamingText("   \n\t  ".into()));
        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "ignored".into(),
                commit_timestamp: "t".into(),
            },
        );
        assert!(s.transcript.is_empty());
        assert!(s.overlay.is_empty());
    }

    #[test]
    fn finalize_turn_commits_visible_partial_like_cancel() {
        let mut s = AppState::new();
        reducer(&mut s, Action::SetStreamingText("final reply".into()));
        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "a-final".into(),
                commit_timestamp: "2026-04-10T00:00:00.000Z".into(),
            },
        );
        assert_eq!(s.transcript.len(), 1);
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => {
                assert_eq!(a.uuid, "a-final");
                assert_eq!(a.timestamp, "2026-04-10T00:00:00.000Z");
            }
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
        assert!(s.overlay.is_empty());
    }

    #[test]
    fn finalize_turn_drops_whitespace_only_partial() {
        let mut s = AppState::new();
        reducer(&mut s, Action::SetStreamingText("  \n\t ".into()));
        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "ignored".into(),
                commit_timestamp: "t".into(),
            },
        );
        assert!(s.transcript.is_empty());
        assert!(s.overlay.is_empty());
    }

    #[test]
    fn finalize_turn_splits_tool_run_from_trailing_text_in_transcript() {
        let mut s = AppState::new();
        reducer(&mut s, start_tool_use("toolu_1"));
        reducer(&mut s, Action::AppendStreamingThinking("reasoning".into()));
        reducer(&mut s, Action::SetStreamingText("done".into()));
        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "a-final".into(),
                commit_timestamp: "t".into(),
            },
        );
        assert_eq!(s.transcript.len(), 2);
        assert!(s.overlay.is_empty());
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => {
                assert_eq!(a.uuid, "a-final");
                assert_eq!(a.message.content.len(), 1);
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::ToolUse(t) if t.id == "toolu_1"
                ));
            }
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
        match &s.transcript.rows()[1] {
            Message::Assistant(a) => {
                assert_eq!(a.uuid, "a-final-part-1");
                assert_eq!(a.message.content.len(), 2);
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::Thinking(t) if t.thinking == "reasoning"
                ));
                assert!(matches!(
                    &a.message.content[1],
                    AssistantContentBlock::Text(t) if t.text == "done"
                ));
            }
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
    }

    #[test]
    fn cancel_with_empty_overlay_is_a_noop_on_transcript() {
        let mut s = AppState::new();
        reducer(&mut s, Action::Commit(user("committed", "first turn")));
        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "ignored".into(),
                commit_timestamp: "t".into(),
            },
        );
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript.rows()[0].uuid(), Some("committed"));
    }

    #[test]
    fn cancel_splits_tool_run_from_trailing_text() {
        // Cancel now preserves all visible overlay content (text +
        // tool uses + thinking) so the user doesn't lose streaming
        // content on ESC / Ctrl+C.
        let mut s = AppState::new();
        reducer(&mut s, start_tool_use("toolu_99"));
        reducer(&mut s, Action::AppendStreamingThinking("reasoning".into()));
        reducer(&mut s, Action::SetStreamingText("real partial".into()));

        let before = s.transcript.len();
        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "p".into(),
                commit_timestamp: "t".into(),
            },
        );
        assert_eq!(
            s.transcript.len(),
            before + 2,
            "tool run and trailing visible text commit as separate assistant messages"
        );
        match &s.transcript.rows()[before] {
            Message::Assistant(a) => {
                assert_eq!(a.message.content.len(), 1, "tool-only chunk");
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::ToolUse(_)
                ));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        match &s.transcript.rows()[before + 1] {
            Message::Assistant(a) => {
                assert_eq!(a.message.content.len(), 2, "thinking + text chunk");
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::Thinking(_)
                ));
                assert!(matches!(
                    &a.message.content[1],
                    AssistantContentBlock::Text(_)
                ));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        assert!(s.overlay.is_empty());
    }

    #[test]
    fn cancel_commits_full_live_shell_history() {
        let mut state = AppState::new();
        reducer(
            &mut state,
            Action::StartToolUse {
                call_id: "shell-1".into(),
                tool_name: "PowerShell".into(),
                kind: ToolKind::Execute,
                initial_status: ToolCallStatus::InProgress,
                initial_title: None,
                raw_input: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        for index in 0..1_000 {
            reducer(
                &mut state,
                Action::UpdateToolUse {
                    call_id: "shell-1".into(),
                    status: Some(ToolCallStatus::InProgress),
                    title: None,
                    content: Some(vec![ToolCallContent::Content(RegularContent {
                        content: ContentBlock::Text(TextContent {
                            text: format!("line-{index:04}"),
                            annotations: None,
                        }),
                    })]),
                    locations: None,
                    raw_output: None,
                },
            );
        }

        reducer(
            &mut state,
            Action::Cancel {
                commit_uuid: "shell-cancel".into(),
                commit_timestamp: "t".into(),
            },
        );

        let Message::Assistant(message) = &state.transcript.rows()[0] else {
            panic!("expected assistant message");
        };
        let AssistantContentBlock::ToolUse(tool) = &message.message.content[0] else {
            panic!("expected shell tool use");
        };
        let content = tool
            .tool_call_content
            .as_ref()
            .expect("cancelled shell content");
        assert_eq!(content.len(), 1_000);
        assert!(matches!(
            &content[0],
            ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(text),
            }) if text.text == "line-0000"
        ));
        assert!(matches!(
            &content[999],
            ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(text),
            }) if text.text == "line-0999"
        ));
        assert!(state.overlay.is_empty());
    }

    #[test]
    fn cancel_marks_inflight_workflow_as_user_interrupted() {
        let mut s = AppState::new();
        let raw_output = json!({
            "workflowProgress": {
                "runId": "run-1",
                "entries": [{
                    "sequence": 1,
                    "entry": {"type": "phase", "title": "Research", "state": "start"}
                }]
            }
        });
        reducer(
            &mut s,
            Action::StartToolUse {
                call_id: "workflow-1".into(),
                tool_name: "Workflow".into(),
                kind: ToolKind::Other,
                initial_status: ToolCallStatus::InProgress,
                initial_title: Some("Workflow".into()),
                raw_input: Some(HashMap::from([("script".into(), json!("workflow()"))])),
                content: None,
                locations: None,
                raw_output: raw_output.as_object().map(|output| {
                    output
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                }),
            },
        );

        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "a-cancel".into(),
                commit_timestamp: "t".into(),
            },
        );

        assert_eq!(s.transcript.len(), 1);
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => match &a.message.content[0] {
                AssistantContentBlock::ToolUse(tool) => {
                    assert_eq!(tool.status, Some(ToolCallStatus::Failed));
                    assert_eq!(tool.raw_output.as_ref(), Some(&raw_output));
                    let content = tool.tool_call_content.as_ref().expect("interrupt content");
                    assert!(content.iter().any(|item| {
                        let ToolCallContent::Content(content) = item else {
                            return false;
                        };
                        let ContentBlock::Text(text) = &content.content else {
                            return false;
                        };
                        text.text == WORKFLOW_INTERRUPTED_MESSAGE
                    }));
                }
                other => panic!("expected workflow tool use, got {other:?}"),
            },
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
        assert!(s.overlay.is_empty());
    }

    /// The `RunWorkflow` alias must get the same interrupt marking as
    /// `Workflow`, or a cancelled run commits a forever-InProgress card.
    #[test]
    fn cancel_marks_inflight_run_workflow_alias_as_user_interrupted() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::StartToolUse {
                call_id: "workflow-1".into(),
                tool_name: "RunWorkflow".into(),
                kind: ToolKind::Other,
                initial_status: ToolCallStatus::InProgress,
                initial_title: Some("Workflow".into()),
                raw_input: Some(HashMap::from([("script".into(), json!("workflow()"))])),
                content: None,
                locations: None,
                raw_output: None,
            },
        );

        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "a-cancel".into(),
                commit_timestamp: "t".into(),
            },
        );

        match &s.transcript.rows()[0] {
            Message::Assistant(a) => match &a.message.content[0] {
                AssistantContentBlock::ToolUse(tool) => {
                    assert_eq!(tool.status, Some(ToolCallStatus::Failed));
                }
                other => panic!("expected workflow tool use, got {other:?}"),
            },
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
    }

    #[test]
    fn finalize_turn_marks_inflight_deferred_workflow_as_interrupted() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::StartToolUse {
                call_id: "workflow-deferred".into(),
                tool_name: "InvokeDeferredTool".into(),
                kind: ToolKind::Other,
                initial_status: ToolCallStatus::InProgress,
                initial_title: Some("Workflow".into()),
                raw_input: Some(HashMap::from([
                    ("tool_name".into(), json!("Workflow")),
                    ("arguments".into(), json!({"script": "workflow()"})),
                ])),
                content: None,
                locations: None,
                raw_output: None,
            },
        );

        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "a-finalize".into(),
                commit_timestamp: "t".into(),
            },
        );

        let Message::Assistant(message) = &s.transcript.rows()[0] else {
            panic!("expected assistant message");
        };
        let AssistantContentBlock::ToolUse(tool) = &message.message.content[0] else {
            panic!("expected workflow tool use");
        };
        assert_eq!(tool.status, Some(ToolCallStatus::Failed));
        assert!(tool.tool_call_content.as_ref().is_some_and(|content| {
            content.iter().any(|item| {
                matches!(
                    item,
                    ToolCallContent::Content(RegularContent {
                        content: ContentBlock::Text(text),
                    }) if text.text == WORKFLOW_INTERRUPTED_MESSAGE
                )
            })
        }));
        assert!(s.overlay.is_empty());
    }

    #[test]
    fn finalize_turn_does_not_rewrite_completed_workflow() {
        let mut s = AppState::new();
        reducer(
            &mut s,
            Action::StartToolUse {
                call_id: "workflow-complete".into(),
                tool_name: "Workflow".into(),
                kind: ToolKind::Other,
                initial_status: ToolCallStatus::Completed,
                initial_title: Some("Workflow".into()),
                raw_input: None,
                content: None,
                locations: None,
                raw_output: Some(HashMap::from([("status".into(), json!("completed"))])),
            },
        );

        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "a-complete".into(),
                commit_timestamp: "t".into(),
            },
        );

        let Message::Assistant(message) = &s.transcript.rows()[0] else {
            panic!("expected assistant message");
        };
        let AssistantContentBlock::ToolUse(tool) = &message.message.content[0] else {
            panic!("expected workflow tool use");
        };
        assert_eq!(tool.status, Some(ToolCallStatus::Completed));
        assert!(tool.tool_call_content.is_none());
    }

    #[test]
    fn cancel_then_cancel_is_idempotent() {
        // Ctrl+C mash — the second cancel must not duplicate the
        // committed row. Pinned by the `push` guard on uuid
        // collision.
        let mut s = AppState::new();
        reducer(&mut s, Action::SetStreamingText("partial".into()));
        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "same-uuid".into(),
                commit_timestamp: "t".into(),
            },
        );
        // Second cancel with empty overlay — nothing to commit.
        reducer(
            &mut s,
            Action::Cancel {
                commit_uuid: "same-uuid".into(),
                commit_timestamp: "t".into(),
            },
        );
        assert_eq!(s.transcript.len(), 1);
    }

    #[test]
    fn finalize_turn_promotes_tool_uses_even_without_text() {
        let mut s = AppState::new();
        reducer(&mut s, start_tool_use("toolu_1"));
        reducer(
            &mut s,
            Action::FinalizeTurn {
                commit_uuid: "a-tools-only".into(),
                commit_timestamp: "t".into(),
            },
        );
        assert_eq!(s.transcript.len(), 1);
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => {
                assert_eq!(a.message.content.len(), 1);
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::ToolUse(t) if t.id == "toolu_1" && t.name == "Read"
                ));
            }
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
        assert!(s.overlay.is_empty());
    }

    fn tool_block_with_name_and_input(
        call_id: &str,
        status: ToolCallStatus,
        tool_name: &str,
        kind: ToolKind,
        raw_input: Option<HashMap<String, Value>>,
    ) -> StreamingContentBlock {
        StreamingContentBlock::ToolUse(StreamingToolUse {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            kind,
            status,
            title: None,
            content: None,
            locations: None,
            raw_input,
            raw_output: None,
        })
    }

    fn tool_block_with_name(
        call_id: &str,
        status: ToolCallStatus,
        tool_name: &str,
        kind: ToolKind,
    ) -> StreamingContentBlock {
        tool_block_with_name_and_input(call_id, status, tool_name, kind, None)
    }

    fn tool_block(call_id: &str, status: ToolCallStatus) -> StreamingContentBlock {
        tool_block_with_name(call_id, status, "Read", ToolKind::Read)
    }

    fn text_block(s: &str) -> StreamingContentBlock {
        StreamingContentBlock::Text(s.into())
    }

    fn thinking_block(s: &str) -> StreamingContentBlock {
        StreamingContentBlock::Thinking(crate::streaming::StreamingThinking {
            thinking: s.into(),
            is_streaming: true,
            streaming_ended_at: None,
        })
    }

    fn closed_thinking_block(s: &str) -> StreamingContentBlock {
        StreamingContentBlock::Thinking(crate::streaming::StreamingThinking {
            thinking: s.into(),
            is_streaming: false,
            streaming_ended_at: Some(0),
        })
    }

    /// Single completed tool with nothing else in the overlay must
    /// stay in the overlay so a sibling tool arriving next can group
    /// with it visually. Without the trailing-cluster hold-back this
    /// would flush to committed and produce the snap-into-group
    /// flicker.
    #[test]
    fn sealed_prefix_holds_back_lone_trailing_completed_tool() {
        let blocks = vec![tool_block("t1", ToolCallStatus::Completed)];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            0
        );
    }

    /// Two completed tools with no text in between must both remain
    /// in the overlay so the streaming segment builder groups them
    /// as a Collapsed run.
    #[test]
    fn sealed_prefix_holds_back_two_consecutive_completed_tools() {
        let blocks = vec![
            tool_block("t1", ToolCallStatus::Completed),
            tool_block("t2", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            0
        );
    }

    /// Mid-stream: completed tool followed by an in-flight tool. The
    /// completed one must NOT flush — they belong in the same
    /// streaming Collapsed group.
    #[test]
    fn sealed_prefix_holds_back_completed_followed_by_in_flight_tool() {
        let blocks = vec![
            tool_block("t1", ToolCallStatus::Completed),
            tool_block("t2", ToolCallStatus::InProgress),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            0
        );
    }

    /// Leading thinking + completed trailing tool: hold back the
    /// whole prefix so thinking stays adjacent to the tool it
    /// preceded.
    #[test]
    fn sealed_prefix_holds_back_thinking_plus_trailing_tool() {
        let blocks = vec![
            thinking_block("reason"),
            tool_block("t1", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            0
        );
    }

    /// Text followed by a completed trailing tool: flush both blocks. The
    /// text is sealed (not the last block), and the text boundary makes the
    /// terminal tool's one-card representation stable enough to commit.
    #[test]
    fn sealed_prefix_flushes_text_but_holds_trailing_tool() {
        let blocks = vec![
            text_block("intro"),
            tool_block("t1", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            2
        );
    }

    /// Tool run terminated by trailing text (still streaming, so
    /// last). Without text being last-and-blocking, we'd flush the
    /// tools and break the group. The existing rule already handles
    /// this — text-as-last halts the prefix at the tool boundary,
    /// then the trailing-cluster pull-back trims further to the prior
    /// Text block (or 0).
    #[test]
    fn sealed_prefix_holds_tools_when_followed_by_streaming_text() {
        let blocks = vec![
            tool_block("t1", ToolCallStatus::Completed),
            tool_block("t2", ToolCallStatus::Completed),
            text_block("commentary"),
        ];
        // last block is Text which is_last, so the loop breaks at
        // i=2 with end=2 (advanced by the two tools); the pull-back
        // then trims back to 0 because there's no Text in [0..2).
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            0
        );
    }

    /// A turn with text → tool cluster → text → in-flight tool. The
    /// first text + tool cluster + middle text are all sealed and
    /// flushable; the in-flight tail keeps the cluster after the
    /// middle text in the overlay.
    #[test]
    fn sealed_prefix_flushes_through_middle_text_keeps_inflight_tail() {
        let blocks = vec![
            text_block("a"),
            tool_block("t1", ToolCallStatus::Completed),
            tool_block("t2", ToolCallStatus::Completed),
            text_block("b"),
            tool_block("t3", ToolCallStatus::InProgress),
        ];
        // Loop: end=1 after a, 2 after t1, 3 after t2, 4 after b
        // (not last), break on t3. end=4. Last meaningful in [0..4)
        // is Text at idx 3, so end stays at 4.
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            4
        );
    }

    /// Empty overlay — trivial.
    #[test]
    fn sealed_prefix_empty_is_zero() {
        assert_eq!(
            sealed_prefix_block_count(&[], SealedPrefixFlushPolicy::HoldBackTrailingToolCluster),
            0
        );
    }

    /// Pending leading tool blocks the whole prefix regardless of
    /// what follows.
    #[test]
    fn sealed_prefix_pending_leading_tool_blocks_everything() {
        let blocks = vec![
            tool_block("t1", ToolCallStatus::InProgress),
            tool_block("t2", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            0
        );
    }

    #[test]
    fn sealed_prefix_inline_drains_groupable_tools_when_non_groupable_tool_boundary_arrives() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
            tool_block_with_name_and_input(
                "edit_1",
                ToolCallStatus::InProgress,
                "Edit",
                ToolKind::Edit,
                Some(HashMap::from([(
                    "file_path".into(),
                    Value::String("crates/rebon-cli/src/tui/prompt_tips.rs".into()),
                )])),
            ),
        ];

        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            3,
            "Edit is a group boundary, so the completed Read/Search group must commit before the edit card streams"
        );
    }

    #[test]
    fn sealed_prefix_inline_releases_trailing_thinking_before_edit_boundary() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
            closed_thinking_block("evaluating reconciliation"),
            tool_block_with_name_and_input(
                "edit_1",
                ToolCallStatus::InProgress,
                "Edit",
                ToolKind::Edit,
                Some(HashMap::from([(
                    "file_path".into(),
                    Value::String("src/lib/library/reconcile.ts".into()),
                )])),
            ),
        ];

        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            4,
            "a visible Edit boundary releases the preceding thinking with the completed Read/Search group"
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_only_trailing_groupable_tool_cluster_after_non_groupable_tool() {
        let blocks = vec![
            text_block("intro"),
            tool_block_with_name_and_input(
                "edit_1",
                ToolCallStatus::Completed,
                "Edit",
                ToolKind::Edit,
                Some(HashMap::from([(
                    "file_path".into(),
                    Value::String("crates/rebon-cli/src/tui/prompt_tips.rs".into()),
                )])),
            ),
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::InProgress),
        ];

        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            2,
            "the completed Edit must commit to transcript/scrollback; only the trailing Read cluster stays live"
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_lone_completed_tool_until_peer_or_text_boundary() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            1
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_completed_tool_group_until_text_boundary_is_sealed() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            1
        );
    }

    #[test]
    fn sealed_prefix_inline_drains_tool_cluster_when_later_assistant_text_is_streaming() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
            text_block("answer"),
        ];
        // The streaming text after the tool cluster IS the assistant
        // boundary — drain intro + both tools so the inline viewport
        // only renders the still-streaming text.
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            3
        );
    }

    #[test]
    fn sealed_prefix_inline_drains_tool_cluster_once_followed_by_sealed_text() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
            text_block("answer"),
            tool_block("bash_1", ToolCallStatus::InProgress),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            4
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_tool_cluster_without_any_following_text() {
        // No text after the tool cluster → hold back (waiting for the
        // assistant boundary that would close the group).
        let blocks = vec![
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            0
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_tool_cluster_when_live_tail_has_more_tools() {
        // An InProgress tool in the live tail does NOT release the held
        // cluster — tools must accumulate so the renderer can group them
        // as "Read 3 files (Ctrl+O to expand)". A non-tool assistant
        // boundary releases completed tools; more tools do not.
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
            tool_block("read_3", ToolCallStatus::InProgress),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            1
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_lone_tool_when_live_tail_has_running_tool() {
        let blocks = vec![
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::InProgress),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            0
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_tool_cluster_when_following_text_is_empty() {
        let blocks = vec![
            tool_block("read_1", ToolCallStatus::Completed),
            text_block("  "),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            0
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_tool_cluster_during_streaming_thinking() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            thinking_block("analyzing results"),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            1
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_tool_cluster_through_closed_thinking() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            closed_thinking_block("analyzing results"),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            1
        );
    }

    #[test]
    fn sealed_prefix_inline_drains_tool_cluster_and_closed_thinking_at_text_boundary() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            closed_thinking_block("analyzing results"),
            text_block("answer"),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            3
        );
    }

    #[test]
    fn flush_inline_policy_holds_tool_cluster_during_streaming_thinking() {
        let mut s = AppState::new();
        s.overlay
            .blocks
            .push(tool_block("read_1", ToolCallStatus::Completed));

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 0);
        assert_eq!(s.overlay.tool_use_count(), 1);

        s.overlay.blocks.push(thinking_block("analyzing results"));
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 0);
        assert_eq!(s.overlay.blocks.len(), 2);
        assert_eq!(s.overlay.tool_use_count(), 1);

        reducer(&mut s, Action::EndStreamingThinking);
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 0);
        assert_eq!(s.overlay.blocks.len(), 2);
        assert_eq!(s.overlay.tool_use_count(), 1);

        s.overlay.blocks.push(text_block("answer"));
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("answer")
        );
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => {
                assert_eq!(a.message.content.len(), 2);
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::ToolUse(t) if t.id == "read_1"
                ));
                assert!(matches!(
                    &a.message.content[1],
                    AssistantContentBlock::Thinking(t) if t.thinking == "analyzing results"
                ));
            }
            other => panic!("expected assistant tool + thinking row, got {other:?}"),
        }
    }

    #[test]
    fn flush_inline_policy_drains_tool_cluster_once_streaming_text_boundary_arrives() {
        let mut s = AppState::new();
        s.overlay.blocks.push(text_block("intro"));
        s.overlay
            .blocks
            .push(tool_block("read_1", ToolCallStatus::Completed));

        // Phase 1: [Text("intro"), Tool(read_1, Completed)]
        // Tool is the only thing after intro — no text boundary yet → hold tool, drain intro only.
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.overlay.tool_use_count(), 1);
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::Text(t) if t.text == "intro"
            )),
            other => panic!("expected assistant text row, got {other:?}"),
        }

        // Phase 2: [Tool(read_1, Completed), Tool(read_2, Completed)]
        // Two tools, no text boundary → still held.
        s.overlay
            .blocks
            .push(tool_block("read_2", ToolCallStatus::Completed));
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.overlay.tool_use_count(), 2);

        // Phase 3: [Tool(read_1, Completed), Tool(read_2, Completed), Text("answer")]
        // Streaming text IS the assistant boundary — drain both tools immediately.
        // This prevents the viewport from cramming tool cards + text + prompt.
        s.overlay.blocks.push(text_block("answer"));
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 3);
        assert_eq!(s.overlay.tool_use_count(), 0);
        assert_eq!(
            s.overlay.combined_streaming_text().as_deref(),
            Some("answer")
        );
        match &s.transcript.rows()[1] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::ToolUse(t) if t.id == "read_1"
            )),
            other => panic!("expected first assistant tool row, got {other:?}"),
        }
        match &s.transcript.rows()[2] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::ToolUse(t) if t.id == "read_2"
            )),
            other => panic!("expected second assistant tool row, got {other:?}"),
        }

        // Phase 4: [Text("answer"), Tool(bash_1, InProgress)]
        // New tool boundary seals the text → drain it.
        s.overlay
            .blocks
            .push(tool_block("bash_1", ToolCallStatus::InProgress));
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert_eq!(s.transcript.len(), 4);
        assert_eq!(s.overlay.tool_use_count(), 1);
        match &s.transcript.rows()[3] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::Text(t) if t.text == "answer"
            )),
            other => panic!("expected assistant text row, got {other:?}"),
        }
    }

    #[test]
    fn sealed_prefix_inline_boundary_policy_keeps_active_thinking_live_after_committed_prefix() {
        let blocks = vec![text_block("intro"), thinking_block("still thinking")];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            1
        );
    }

    #[test]
    fn sealed_prefix_inline_boundary_policy_flushes_closed_thinking_before_later_text() {
        let blocks = vec![
            closed_thinking_block("stable reasoning"),
            text_block("answer"),
            tool_block("read_1", ToolCallStatus::Completed),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            2
        );
    }

    #[test]
    fn flush_inline_boundary_policy_holds_standalone_closed_thinking() {
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingThinking("reasoning".into()));
        reducer(&mut s, Action::EndStreamingThinking);

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy:
                    SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary,
            },
        );

        assert!(s.transcript.is_empty());
        assert_eq!(s.overlay.blocks.len(), 1);
        assert!(matches!(
            &s.overlay.blocks[0],
            StreamingContentBlock::Thinking(t)
                if t.thinking == "reasoning" && !t.is_streaming
        ));
    }

    #[test]
    fn sealed_prefix_inline_holds_only_uninterrupted_thinking_run() {
        let thinking_run = vec![
            closed_thinking_block("first step"),
            closed_thinking_block("second step"),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &thinking_run,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            0
        );

        let followed_by_visible_tool = vec![
            closed_thinking_block("first step"),
            tool_block_with_name(
                "bash_1",
                ToolCallStatus::InProgress,
                "Bash",
                ToolKind::Execute,
            ),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &followed_by_visible_tool,
                SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
            ),
            1,
            "a visible tool must release the preceding thinking into scrollback"
        );
    }

    #[test]
    fn sealed_prefix_screen_still_holds_trailing_tool_cluster_with_later_streaming_text() {
        let blocks = vec![
            tool_block("read_1", ToolCallStatus::Completed),
            tool_block("read_2", ToolCallStatus::Completed),
            text_block("streaming tail"),
        ];
        assert_eq!(
            sealed_prefix_block_count(
                &blocks,
                SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
            ),
            0
        );
    }

    #[test]
    fn flush_inline_policy_commits_closed_unanchored_thinking() {
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingThinking("reasoning".into()));
        reducer(&mut s, Action::EndStreamingThinking);

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStable,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        assert!(s.overlay.is_empty());
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => {
                assert_eq!(a.message.content.len(), 1);
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::Thinking(t) if t.thinking == "reasoning"
                ));
            }
            other => panic!("expected assistant thinking row, got {other:?}"),
        }
    }

    #[test]
    fn sealed_prefix_inline_keeps_active_thinking_live_after_committed_prefix() {
        let blocks = vec![text_block("intro"), thinking_block("still thinking")];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            1
        );
    }

    #[test]
    fn sealed_prefix_inline_treats_closed_thinking_as_flushable_output() {
        let blocks = vec![
            closed_thinking_block("stable reasoning"),
            text_block("tail"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            2
        );
    }

    #[test]
    fn sealed_prefix_inline_drains_completed_groupable_tool_before_later_text() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            text_block("next assistant text"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            3
        );
    }

    #[test]
    fn sealed_prefix_inline_drains_closed_thinking_before_later_tool() {
        let blocks = vec![
            closed_thinking_block("stable reasoning"),
            tool_block("read_1", ToolCallStatus::Completed),
            text_block("tail"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            3
        );
    }

    #[test]
    fn flush_inline_policy_attaches_closed_thinking_to_following_text_anchor() {
        let mut s = AppState::new();
        reducer(&mut s, Action::AppendStreamingThinking("reasoning".into()));
        reducer(&mut s, Action::EndStreamingThinking);
        reducer(&mut s, Action::SetStreamingText("answer".into()));
        s.overlay.blocks.push(text_block("next anchor"));

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStable,
            },
        );

        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.overlay.blocks.len(), 1, "last text remains live");
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => {
                assert_eq!(a.message.content.len(), 2);
                assert!(matches!(
                    &a.message.content[0],
                    AssistantContentBlock::Thinking(t) if t.thinking == "reasoning"
                ));
                assert!(matches!(
                    &a.message.content[1],
                    AssistantContentBlock::Text(t) if t.text == "answer"
                ));
            }
            other => panic!("expected assistant row with thinking + text, got {other:?}"),
        }
    }

    #[test]
    fn sealed_prefix_inline_drains_non_groupable_tool_before_active_thinking() {
        let blocks = vec![
            text_block("intro"),
            tool_block_with_name(
                "agent_1",
                ToolCallStatus::Completed,
                "Agent",
                ToolKind::Other,
            ),
            thinking_block("next step"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            2
        );
    }

    #[test]
    fn sealed_prefix_inline_drains_trailing_groupable_tool_before_active_thinking() {
        let blocks = vec![
            text_block("intro"),
            tool_block("read_1", ToolCallStatus::Completed),
            thinking_block("maybe more reads"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            2
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_shell_search_tool_missed_by_legacy_heuristic() {
        let blocks = vec![
            text_block("intro"),
            tool_block_with_name_and_input(
                "bash_1",
                ToolCallStatus::Completed,
                "Bash",
                ToolKind::Execute,
                Some(HashMap::from([(
                    "command".into(),
                    Value::String("rg semantic-risk crates/rebon-tui/src/state.rs".into()),
                )])),
            ),
            thinking_block("maybe more searches"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            2
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_absorbed_toolsearch_tool() {
        let blocks = vec![
            text_block("intro"),
            tool_block_with_name(
                "toolsearch_1",
                ToolCallStatus::Completed,
                "ToolSearch",
                ToolKind::Other,
            ),
            thinking_block("maybe more tool discovery"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            2
        );
    }

    #[test]
    fn sealed_prefix_inline_holds_mcp_tool() {
        let blocks = vec![
            text_block("intro"),
            tool_block_with_name(
                "mcp_1",
                ToolCallStatus::Completed,
                "mcp__slack__read_messages",
                ToolKind::Other,
            ),
            thinking_block("maybe more mcp calls"),
        ];
        assert_eq!(
            sealed_prefix_block_count(&blocks, SealedPrefixFlushPolicy::DrainClosedStable),
            2
        );
    }

    #[test]
    fn flush_inline_policy_commits_agent_summary_leaves_active_thinking_live() {
        let mut s = AppState::new();
        s.overlay.blocks.push(text_block("intro"));
        s.overlay.blocks.push(tool_block_with_name(
            "agent_1",
            ToolCallStatus::Completed,
            "Agent",
            ToolKind::Other,
        ));
        s.overlay.blocks.push(thinking_block("next step"));

        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::DrainClosedStable,
            },
        );

        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.overlay.blocks.len(), 1);
        assert!(matches!(
            &s.overlay.blocks[0],
            StreamingContentBlock::Thinking(t) if t.thinking == "next step" && t.is_streaming
        ));
        match &s.transcript.rows()[0] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::Text(t) if t.text == "intro"
            )),
            other => panic!("expected assistant text row, got {other:?}"),
        }
        match &s.transcript.rows()[1] {
            Message::Assistant(a) => assert!(matches!(
                &a.message.content[0],
                AssistantContentBlock::ToolUse(t) if t.name == "Agent" && t.id == "agent_1"
            )),
            other => panic!("expected assistant tool row, got {other:?}"),
        }
    }

    /// Integration-shaped check: drive the reducer's flush path and
    /// confirm two completed tools in a row don't get split across
    /// the overlay/committed boundary. This is the actual user-
    /// visible behavior the bug report described.
    #[test]
    fn flush_keeps_two_completed_tools_grouped_in_overlay() {
        let mut s = AppState::new();
        // tool_1 starts and completes
        reducer(&mut s, start_tool_use("toolu_1"));
        reducer(
            &mut s,
            Action::UpdateToolUse {
                call_id: "toolu_1".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        // Flush attempt — must NOT promote tool_1, since it's the
        // trailing block.
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::HoldBackTrailingToolCluster,
            },
        );
        assert_eq!(s.transcript.len(), 0, "trailing tool must stay in overlay");
        assert_eq!(s.overlay.tool_use_count(), 1);

        // tool_2 starts and completes
        reducer(&mut s, start_tool_use("toolu_2"));
        reducer(
            &mut s,
            Action::UpdateToolUse {
                call_id: "toolu_2".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        reducer(
            &mut s,
            Action::FlushSealedPrefix {
                commit_timestamp: "t".into(),
                policy: SealedPrefixFlushPolicy::HoldBackTrailingToolCluster,
            },
        );
        // Both tools must remain in the overlay so the streaming
        // segment builder can group them as a Collapsed run.
        assert_eq!(
            s.transcript.len(),
            0,
            "neither tool should flush while the cluster is trailing"
        );
        assert_eq!(s.overlay.tool_use_count(), 2);
    }
}
