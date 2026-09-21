use super::*;
use std::{collections::VecDeque, sync::Arc};

// `ToolOutputVerbosity` moved to the shared `rebon-render::kind` so the
// GPUI app passes the same verbosity into shared body builders (workflow_body_lines
// etc.). Re-exported so `mod.rs pub use cache::{ToolOutputVerbosity, …}` and the
// in-crate callers resolve unchanged.
pub use rebon_render::kind::ToolOutputVerbosity;

/// Cached transcript measurements reused across frames.
///
/// Row-level invalidation is keyed by `(uuid, width, row_revision,
/// live_activity_signature, add_margin, last_thinking_block_id)` inside
/// `row_heights`, so per-message mutations (`upsert`, append) and async-Agent
/// activity updates only miss the rows that actually changed. The fields below
/// carry transcript-wide state that flips every row in lockstep; those still
/// clear the cache wholesale.
#[derive(Debug, Clone, Default)]
pub struct TranscriptMeasureCache {
    pub(super) row_heights: MeasureCache,
    pub(super) collapsed_group_heights: HashMap<CollapsedMeasureKey, u16>,
    pub(super) streaming_overlay: StreamingOverlayRenderCache,
    pub(super) layout: Option<Arc<TranscriptLayout>>,
    pub(super) clipped_segments: ClippedSegmentRenderCache,
    verbosity: Option<ToolOutputVerbosity>,
    render_thinking_only_rows: bool,
    expand_thinking_rows: bool,
    force_verbose_edit_tool_previews: bool,
    static_agent_group_status: bool,
    inline_live_workflow_card_max_rows: Option<u16>,
    math_layout_enabled: bool,
    #[cfg(test)]
    pub(super) layout_cache_hits: usize,
    #[cfg(test)]
    pub(super) layout_full_builds: usize,
    #[cfg(test)]
    pub(super) layout_incremental_appends: usize,
    #[cfg(test)]
    pub(super) layout_tail_updates: usize,
    #[cfg(test)]
    pub(super) layout_activity_updates: usize,
    #[cfg(test)]
    pub(super) clipped_segment_hits: usize,
}

impl TranscriptMeasureCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.row_heights.clear();
        self.collapsed_group_heights.clear();
        self.streaming_overlay.clear();
        self.layout = None;
        self.clipped_segments.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.row_heights.is_empty()
            && self.collapsed_group_heights.is_empty()
            && self.layout.is_none()
            && self.clipped_segments.is_empty()
    }

    pub(super) fn prepare(
        &mut self,
        verbosity: ToolOutputVerbosity,
        math_layout_enabled: bool,
        extras: TranscriptRenderExtras<'_>,
    ) {
        if self.verbosity != Some(verbosity)
            || self.render_thinking_only_rows != extras.render_thinking_only_rows
            || self.expand_thinking_rows != extras.expand_thinking_rows
            || self.force_verbose_edit_tool_previews != extras.force_verbose_edit_tool_previews
            || self.static_agent_group_status != extras.static_agent_group_status
            || self.inline_live_workflow_card_max_rows != extras.inline_live_workflow_card_max_rows
            || self.math_layout_enabled != math_layout_enabled
        {
            self.clear();
            self.verbosity = Some(verbosity);
            self.render_thinking_only_rows = extras.render_thinking_only_rows;
            self.expand_thinking_rows = extras.expand_thinking_rows;
            self.force_verbose_edit_tool_previews = extras.force_verbose_edit_tool_previews;
            self.static_agent_group_status = extras.static_agent_group_status;
            self.inline_live_workflow_card_max_rows = extras.inline_live_workflow_card_max_rows;
            self.math_layout_enabled = math_layout_enabled;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct StreamingOverlayRenderCache;

impl StreamingOverlayRenderCache {
    pub fn new() -> Self {
        Self
    }

    pub fn clear(&mut self) {}
}

#[derive(Debug, Clone, Copy)]
pub(super) enum StreamingOverlayRenderMode {
    Measure,
    Paint,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct StreamingOverlayRenderOptions {
    pub(super) mode: StreamingOverlayRenderMode,
    pub(super) native_math_graphics: bool,
    /// Whether to reserve one blank row before the first visible overlay
    /// segment. Full transcript rendering enables this only when there is
    /// committed content above the live overlay; inline overlays and
    /// standalone overlay renders start on the first row.
    pub(super) leading_margin: bool,
}

impl StreamingOverlayRenderOptions {
    pub(super) fn paint() -> Self {
        Self {
            mode: StreamingOverlayRenderMode::Paint,
            native_math_graphics: true,
            leading_margin: false,
        }
    }

    pub(super) fn paint_after_committed() -> Self {
        Self {
            mode: StreamingOverlayRenderMode::Paint,
            native_math_graphics: true,
            leading_margin: true,
        }
    }

    pub(super) fn measure(leading_margin: bool) -> Self {
        Self {
            mode: StreamingOverlayRenderMode::Measure,
            native_math_graphics: false,
            leading_margin,
        }
    }

    pub(super) fn scratch_paint(leading_margin: bool) -> Self {
        Self {
            mode: StreamingOverlayRenderMode::Paint,
            native_math_graphics: false,
            leading_margin,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct CollapsedMeasureKey {
    pub(super) first_index: usize,
    pub(super) last_index: usize,
    pub(super) len: usize,
    pub(super) revision_hash: u64,
    pub(super) width: u16,
    pub(super) show_hint_lines: bool,
    /// The measured height includes a leading margin row when the group is
    /// the first segment, so the key must include `add_margin` too — mirrors
    /// the row-level `MeasureKey`. Without it a persisted cache could reuse a
    /// height computed for the other margin state and mis-size the first row.
    pub(super) add_margin: bool,
}

impl CollapsedMeasureKey {
    pub(super) fn new(
        rows: &[Message],
        indices: &[usize],
        row_revisions: &[u64],
        width: u16,
        show_hint_lines: bool,
        add_margin: bool,
    ) -> Self {
        debug_assert!(!indices.is_empty());
        let revision_hash = hash_row_revision_indices(rows, indices, row_revisions);
        Self {
            first_index: indices[0],
            last_index: indices[indices.len() - 1],
            len: indices.len(),
            revision_hash,
            width,
            show_hint_lines,
            add_margin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptLayoutRenderKey {
    pub(super) width: u16,
    pub(super) verbosity: ToolOutputVerbosity,
    pub(super) divider_before_index: Option<usize>,
    pub(super) show_running_transcript_hints: bool,
    pub(super) leading_segment_margin: bool,
    pub(super) render_thinking_only_rows: bool,
    pub(super) expand_thinking_rows: bool,
    pub(super) force_verbose_edit_tool_previews: bool,
    pub(super) static_agent_group_status: bool,
    pub(super) inline_live_workflow_card_max_rows: Option<u16>,
    pub(super) math_layout_enabled: bool,
    pub(super) latest_visible_thinking_block_id: Option<String>,
}

impl TranscriptLayoutRenderKey {
    pub(super) fn new(
        width: u16,
        verbosity: ToolOutputVerbosity,
        divider_before_index: Option<usize>,
        show_running_transcript_hints: bool,
        math_layout_enabled: bool,
        latest_visible_thinking_block_id: Option<String>,
        extras: TranscriptRenderExtras<'_>,
    ) -> Self {
        Self {
            width,
            verbosity,
            divider_before_index,
            show_running_transcript_hints,
            math_layout_enabled,
            leading_segment_margin: extras.leading_segment_margin,
            render_thinking_only_rows: extras.render_thinking_only_rows,
            expand_thinking_rows: extras.expand_thinking_rows,
            force_verbose_edit_tool_previews: extras.force_verbose_edit_tool_previews,
            static_agent_group_status: extras.static_agent_group_status,
            inline_live_workflow_card_max_rows: extras.inline_live_workflow_card_max_rows,
            latest_visible_thinking_block_id,
        }
    }

    pub(super) fn matches_without_latest_thinking(
        &self,
        width: u16,
        verbosity: ToolOutputVerbosity,
        divider_before_index: Option<usize>,
        show_running_transcript_hints: bool,
        math_layout_enabled: bool,
        extras: TranscriptRenderExtras<'_>,
    ) -> bool {
        self.width == width
            && self.verbosity == verbosity
            && self.divider_before_index == divider_before_index
            && self.show_running_transcript_hints == show_running_transcript_hints
            && self.math_layout_enabled == math_layout_enabled
            && self.leading_segment_margin == extras.leading_segment_margin
            && self.render_thinking_only_rows == extras.render_thinking_only_rows
            && self.expand_thinking_rows == extras.expand_thinking_rows
            && self.force_verbose_edit_tool_previews == extras.force_verbose_edit_tool_previews
            && self.static_agent_group_status == extras.static_agent_group_status
            && self.inline_live_workflow_card_max_rows == extras.inline_live_workflow_card_max_rows
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TranscriptRowSignature {
    pub(super) uuid: Option<String>,
    pub(super) revision: u64,
    pub(super) sticky_anchor: bool,
    pub(super) live_activity_signature: u64,
}

impl TranscriptRowSignature {
    pub(super) fn collect(
        rows: &[Message],
        row_revisions: &[u64],
        extras: TranscriptRenderExtras<'_>,
    ) -> Vec<Self> {
        rows.iter()
            .enumerate()
            .map(|(idx, row)| Self {
                uuid: row.uuid().map(str::to_string),
                revision: row_revisions.get(idx).copied().unwrap_or(0),
                sticky_anchor: message_has_sticky_anchor_preview(row),
                live_activity_signature: message_live_agent_activity_signature(row, extras),
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(super) struct TranscriptLayout {
    pub(super) render_key: TranscriptLayoutRenderKey,
    pub(super) transcript_revision: u64,
    pub(super) row_count: usize,
    pub(super) row_signatures: Vec<TranscriptRowSignature>,
    pub(super) segments: Vec<TranscriptSegment>,
    pub(super) seg_heights: Vec<u16>,
    pub(super) segment_slot_tops: Vec<usize>,
    pub(super) segment_tops: Vec<usize>,
    pub(super) segment_bottoms: Vec<usize>,
    pub(super) segment_height_prefix: Vec<usize>,
    pub(super) total_msg_lines: usize,
    pub(super) divider_before_segment: Option<usize>,
    pub(super) running_hint_segment: Option<usize>,
    pub(super) sticky_anchors: Vec<Option<TranscriptStickyAnchor>>,
}

impl TranscriptLayout {
    pub(super) fn new(
        render_key: TranscriptLayoutRenderKey,
        transcript_revision: u64,
        row_signatures: Vec<TranscriptRowSignature>,
        segments: Vec<TranscriptSegment>,
        seg_heights: Vec<u16>,
        running_hint_segment: Option<usize>,
    ) -> Self {
        let row_count = row_signatures.len();
        let mut layout = Self {
            render_key,
            transcript_revision,
            row_count,
            row_signatures,
            segments,
            seg_heights,
            segment_slot_tops: Vec::new(),
            segment_tops: Vec::new(),
            segment_bottoms: Vec::new(),
            segment_height_prefix: Vec::new(),
            total_msg_lines: 0,
            divider_before_segment: None,
            running_hint_segment,
            sticky_anchors: Vec::new(),
        };
        layout.recompute_metrics();
        layout
    }

    pub(super) fn recompute_metrics(&mut self) {
        self.divider_before_segment = locate_divider_before_segment(
            &self.segments,
            self.row_count,
            self.render_key.divider_before_index,
        );
        self.segment_slot_tops.clear();
        self.segment_tops.clear();
        self.segment_bottoms.clear();
        self.segment_height_prefix.clear();
        self.sticky_anchors.clear();
        self.segment_slot_tops.reserve(self.segments.len());
        self.segment_tops.reserve(self.segments.len());
        self.segment_bottoms.reserve(self.segments.len());
        self.segment_height_prefix.reserve(self.segments.len() + 1);
        self.sticky_anchors.reserve(self.segments.len());

        let mut line = 0usize;
        let mut height_sum = 0usize;
        let mut sticky_anchor = None;
        self.segment_height_prefix.push(0);
        for (idx, height) in self.seg_heights.iter().copied().enumerate() {
            self.segment_slot_tops.push(line);
            if let Some(row_index) =
                first_sticky_anchor_row(&self.row_signatures, &self.segments[idx])
            {
                sticky_anchor = Some(TranscriptStickyAnchor {
                    row_index,
                    scroll_offset: line,
                });
            }
            self.sticky_anchors.push(sticky_anchor);
            if self.divider_before_segment == Some(idx) {
                line = line.saturating_add(1);
            }
            self.segment_tops.push(line);
            line = line.saturating_add(height as usize);
            self.segment_bottoms.push(line);
            height_sum = height_sum.saturating_add(height as usize);
            self.segment_height_prefix.push(height_sum);
        }
        self.total_msg_lines = line;
    }

    pub(super) fn first_visible_segment(
        &self,
        scroll_offset: usize,
    ) -> Option<TranscriptVisibleStart> {
        let idx = self
            .segment_bottoms
            .partition_point(|bottom| *bottom <= scroll_offset);
        if idx >= self.segments.len() {
            return None;
        }
        let segment_top = self.segment_tops[idx];
        let slot_top = self.segment_slot_tops[idx];
        let start_idx_shows_divider = self.divider_before_segment == Some(idx)
            && scroll_offset == slot_top
            && slot_top < segment_top;
        let skip_lines_in_first = if start_idx_shows_divider {
            0
        } else {
            scroll_offset.saturating_sub(segment_top)
        };
        Some(TranscriptVisibleStart {
            segment_idx: idx,
            first_visible_segment_top: segment_top,
            skip_lines_in_first,
            start_idx_shows_divider,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TranscriptVisibleStart {
    pub(super) segment_idx: usize,
    pub(super) first_visible_segment_top: usize,
    pub(super) skip_lines_in_first: usize,
    pub(super) start_idx_shows_divider: bool,
}

fn locate_divider_before_segment(
    segments: &[TranscriptSegment],
    row_count: usize,
    divider_before_index: Option<usize>,
) -> Option<usize> {
    let row_idx = divider_before_index?;
    if row_idx >= row_count {
        return None;
    }
    for (seg_idx, seg) in segments.iter().enumerate() {
        if seg.first() >= row_idx || seg.contains(row_idx) {
            return Some(seg_idx);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClippedSegmentCacheKey {
    pub(super) signature: ClippedSegmentSignature,
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) verbosity: ToolOutputVerbosity,
    pub(super) theme_name: ThemeName,
    pub(super) live_agent_elapsed_signature: Option<u64>,
    pub(super) supports_hyperlinks: bool,
    pub(super) math_display: u64,
    pub(super) add_margin: bool,
    pub(super) show_collapsed_hint_lines: bool,
    pub(super) last_thinking_block_id: Option<String>,
    pub(super) live_activity_signature: u64,
    pub(super) render_thinking_only_rows: bool,
    pub(super) expand_thinking_rows: bool,
    pub(super) force_verbose_edit_tool_previews: bool,
    pub(super) static_agent_group_status: bool,
    pub(super) inline_live_workflow_card_max_rows: Option<u16>,
}

impl ClippedSegmentCacheKey {
    pub(super) fn new(
        rows: &[Message],
        row_revisions: &[u64],
        segment: &TranscriptSegment,
        width: u16,
        height: u16,
        verbosity: ToolOutputVerbosity,
        theme: &RenderTheme,
        add_margin: bool,
        show_collapsed_hint_lines: bool,
        last_thinking_block_id: Option<&str>,
        extras: TranscriptRenderExtras<'_>,
        render_time_ms: u64,
    ) -> Self {
        Self {
            signature: ClippedSegmentSignature::new(rows, row_revisions, segment),
            width,
            height,
            verbosity,
            theme_name: theme.name,
            live_agent_elapsed_signature: if verbosity == ToolOutputVerbosity::Compact {
                agent_group_running_elapsed_signature(rows, segment, width, extras, render_time_ms)
            } else {
                None
            },
            supports_hyperlinks: theme.supports_hyperlinks,
            math_display: theme.math_display.cache_discriminant(),
            add_margin,
            show_collapsed_hint_lines,
            last_thinking_block_id: last_thinking_block_id.map(str::to_string),
            live_activity_signature: segment_live_agent_activity_signature(rows, segment, extras),
            render_thinking_only_rows: extras.render_thinking_only_rows,
            expand_thinking_rows: extras.expand_thinking_rows,
            force_verbose_edit_tool_previews: extras.force_verbose_edit_tool_previews,
            static_agent_group_status: extras.static_agent_group_status,
            inline_live_workflow_card_max_rows: extras.inline_live_workflow_card_max_rows,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClippedSegmentSignature {
    pub(super) kind: u8,
    pub(super) first_index: usize,
    pub(super) last_index: usize,
    pub(super) len: usize,
    pub(super) revision_hash: u64,
}

impl ClippedSegmentSignature {
    fn new(rows: &[Message], row_revisions: &[u64], segment: &TranscriptSegment) -> Self {
        let (kind, indices): (u8, Vec<usize>) = match segment {
            TranscriptSegment::Single(idx) => (0, vec![*idx]),
            TranscriptSegment::Collapsed { indices } => (1, indices.clone()),
            TranscriptSegment::AgentGroup { indices } => (2, indices.clone()),
            TranscriptSegment::ThinkingGroup { .. } => (3, segment.row_indices()),
        };
        let first_index = indices.first().copied().unwrap_or(0);
        let last_index = indices.last().copied().unwrap_or(first_index);
        let len = indices.len();
        let revision_hash = hash_row_revision_indices(rows, &indices, row_revisions);
        Self {
            kind,
            first_index,
            last_index,
            len,
            revision_hash,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ClippedSegmentCacheEntry {
    key: ClippedSegmentCacheKey,
    buffer: Arc<Buffer>,
    cells: usize,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ClippedSegmentRenderCache {
    entries: VecDeque<ClippedSegmentCacheEntry>,
    cells: usize,
}

impl ClippedSegmentRenderCache {
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.cells = 0;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn get(&mut self, key: &ClippedSegmentCacheKey) -> Option<Arc<Buffer>> {
        let idx = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(idx)?;
        let buffer = Arc::clone(&entry.buffer);
        self.entries.push_back(entry);
        Some(buffer)
    }

    pub(super) fn insert(&mut self, key: ClippedSegmentCacheKey, buffer: Buffer) -> Arc<Buffer> {
        let cells = usize::from(key.width) * usize::from(key.height);
        let buffer = Arc::new(buffer);
        if cells == 0 || cells > CLIPPED_SEGMENT_CACHE_MAX_ENTRY_CELLS {
            return buffer;
        }

        if let Some(idx) = self.entries.iter().position(|entry| entry.key == key) {
            if let Some(entry) = self.entries.remove(idx) {
                self.cells = self.cells.saturating_sub(entry.cells);
            }
        }

        self.cells = self.cells.saturating_add(cells);
        self.entries.push_back(ClippedSegmentCacheEntry {
            key,
            buffer: Arc::clone(&buffer),
            cells,
        });

        while self.entries.len() > CLIPPED_SEGMENT_CACHE_MAX_ENTRIES
            || self.cells > CLIPPED_SEGMENT_CACHE_MAX_CELLS
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.cells = self.cells.saturating_sub(entry.cells);
        }

        buffer
    }
}

fn message_has_sticky_anchor_preview(row: &Message) -> bool {
    let Message::User(message) = row else {
        return false;
    };
    if message.is_meta == Some(true) {
        return false;
    }
    message.message.content.iter().any(|block| match block {
        UserContentBlock::Text(text) => text.text.split_whitespace().next().is_some(),
        UserContentBlock::Image(_) => true,
        UserContentBlock::ToolResult(_) => false,
    })
}

fn first_sticky_anchor_row(
    row_signatures: &[TranscriptRowSignature],
    segment: &TranscriptSegment,
) -> Option<usize> {
    match segment {
        TranscriptSegment::Single(i) => row_signatures
            .get(*i)
            .and_then(|signature| signature.sticky_anchor.then_some(*i)),
        TranscriptSegment::Collapsed { indices } | TranscriptSegment::AgentGroup { indices } => {
            indices.iter().copied().find(|idx| {
                row_signatures
                    .get(*idx)
                    .is_some_and(|signature| signature.sticky_anchor)
            })
        }
        TranscriptSegment::ThinkingGroup { segments } => segments
            .iter()
            .find_map(|segment| first_sticky_anchor_row(row_signatures, segment)),
    }
}

fn hash_row_revision_indices(rows: &[Message], indices: &[usize], row_revisions: &[u64]) -> u64 {
    let mut revision_hash = 0xcbf29ce484222325u64;
    for &idx in indices {
        revision_hash ^= idx as u64;
        revision_hash = revision_hash.wrapping_mul(0x100000001b3);
        if let Some(uuid) = rows.get(idx).and_then(Message::uuid) {
            for byte in uuid.as_bytes() {
                revision_hash ^= u64::from(*byte);
                revision_hash = revision_hash.wrapping_mul(0x100000001b3);
            }
        }
        revision_hash ^= row_revisions.get(idx).copied().unwrap_or(0);
        revision_hash = revision_hash.wrapping_mul(0x100000001b3);
    }
    revision_hash
}

const CLIPPED_SEGMENT_CACHE_MAX_ENTRIES: usize = 3;
const CLIPPED_SEGMENT_CACHE_MAX_ENTRY_CELLS: usize = 120_000;
const CLIPPED_SEGMENT_CACHE_MAX_CELLS: usize = 300_000;
pub(super) const TRANSCRIPT_MEASURE_HEIGHT: u16 = 500;
