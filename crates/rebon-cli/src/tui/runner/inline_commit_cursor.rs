//! Inline-mode scrollback committer. Tracks which transcript rows have
//! already been emitted into the terminal scrollback (via
//! `Terminal::insert_before`) so that subsequent draws only render the
//! still-live tail, and flushes new rows out of the live region into
//! scrollback once the transcript appends. Self-contained over
//! `AppState` and `rebon_tui`; touches no other runner state.

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use rebon_width::WidthStr;
use sha2::{Digest, Sha256};

use rebon_tui::RenderTheme;

use crate::tui::app::AppState;

/// Insert content above the inline viewport so it lands in scrollback,
/// using `Buffer::diff` for the visible draw step so wide-char shadow
/// cells are not emitted as literal spaces.
///
/// Background: with the `scrolling-regions` ratatui feature disabled
/// (so newline-based scrolling pushes rows into the terminal's actual
/// scrollback on Windows Terminal / conhost), ratatui's internal
/// `draw_lines` writes every cell — including the width-0 shadow cell
/// that `Buffer::set_stringn` places after each width-2 character —
/// which the crossterm backend renders as `Print(" ")` and so injects
/// a literal space between every CJK glyph in scrollback.
///
/// Workaround: let `Terminal::insert_before` handle the scroll +
/// `viewport_area` bookkeeping by passing it a no-op closure (its
/// broken `draw_lines` then writes only empty cells = `" "`, which is
/// harmless), then write our pre-rendered content into the now-cleared
/// area immediately above the post-call viewport via `Buffer::diff`,
/// which correctly skips wide-char shadow cells.
pub(super) fn insert_before_cjk_safe<B, F>(
    terminal: &mut ratatui::Terminal<B>,
    height: u16,
    draw_fn: F,
) -> std::io::Result<()>
where
    B: Backend,
    F: FnOnce(&mut Buffer),
{
    if height == 0 {
        return Ok(());
    }
    let viewport_width = terminal.get_frame().area().width;
    if viewport_width == 0 {
        return Ok(());
    }

    let content_area = Rect::new(0, 0, viewport_width, height);
    let mut content_buf = Buffer::empty(content_area);
    draw_fn(&mut content_buf);
    mark_wide_shadow_cells_skip(&mut content_buf);

    // Chunk so each call fits in the above-viewport space and ratatui's
    // empty draw_lines never overflows real content into scrollback. We
    // can't pass the real (CJK) buffer to insert_before in the normal case
    // because its draw_lines writes every cell; the skip markers above stop
    // wide-char shadow cells from being printed as literal spaces when we do
    // need to pass real content through.
    //
    // When the inline viewport already starts at row 0 there is no visible
    // gap to refill after a no-op insert. The old no-op path inserted blank
    // rows, scrolled those blanks into scrollback, then had nowhere to draw
    // the real chunk, which made committed output disappear behind the live
    // viewport. In that full-height case, hand the pre-rendered chunk directly
    // to insert_before so the content itself is what scrolls out.
    let row_stride = viewport_width as usize;
    let mut remaining = &content_buf.content[..];
    let mut rows_left = height;

    while rows_left > 0 {
        let viewport_top_before = terminal.get_frame().area().top();
        let max_chunk = viewport_top_before.max(1).min(rows_left);
        let chunk_h = max_chunk;
        let (chunk_cells, rest) = remaining.split_at(chunk_h as usize * row_stride);

        if viewport_top_before == 0 {
            terminal.insert_before(chunk_h, |buf| {
                buf.content.clone_from_slice(chunk_cells);
            })?;
            remaining = rest;
            rows_left = rows_left.saturating_sub(chunk_h);
            continue;
        }

        terminal.insert_before(chunk_h, |_| {})?;

        let viewport_top_after = terminal.get_frame().area().top();
        let Some(draw_top) = viewport_top_after.checked_sub(chunk_h) else {
            // Viewport sits at row 0 with no room above — `insert_before`
            // shifted it down by `chunk_h` instead of pushing rows into
            // scrollback. Continue with the next chunk; the next iteration's
            // viewport_top will be `chunk_h`, so we'll have room.
            remaining = rest;
            rows_left = rows_left.saturating_sub(chunk_h);
            continue;
        };

        let draw_area = Rect::new(0, draw_top, viewport_width, chunk_h);
        let old = Buffer::empty(draw_area);
        let mut new_buf = Buffer::empty(draw_area);
        new_buf.content.clone_from_slice(chunk_cells);
        let updates = old.diff(&new_buf);

        let backend = terminal.backend_mut();
        backend.draw(updates.into_iter())?;

        remaining = rest;
        rows_left = rows_left.saturating_sub(chunk_h);
    }

    // Coalesce the per-chunk flush into a single flush for the whole rebuild.
    // Every chunk's bytes — the `insert_before` scrolls (which `draw_lines`
    // flushes internally) and our own `backend.draw` cell updates — are queued
    // in order through the same `BufWriter`, so one trailing flush delivers the
    // remainder while dropping the redundant explicit per-chunk flush. On an
    // O(transcript) resize rebuild that shortens the wall-clock spent inside the
    // BSU/ESU synchronized frame, shrinking the window in which a half-applied
    // repaint could surface as a flash.
    terminal.backend_mut().flush()?;
    Ok(())
}

fn mark_wide_shadow_cells_skip(buf: &mut Buffer) {
    let area = buf.area;
    for y in area.y..area.bottom() {
        let mut covered_until = area.x;
        for x in area.x..area.right() {
            let cell = &mut buf[(x, y)];
            if x < covered_until && cell.symbol() == " " {
                cell.set_skip(true);
                continue;
            }
            let width = cell.symbol().width() as u16;
            covered_until = x.saturating_add(width.max(1));
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct InlineRuntimeState {
    pub(super) commit_cursor: InlineCommitCursor,
    last_terminal_size: Option<(u16, u16)>,
    /// User-configured viewport height (the value passed to
    /// `Viewport::Inline(h)` at startup). Acts as the floor when the
    /// event loop dynamically resizes the viewport — we'll grow above
    /// this but never shrink below it, so quiet sessions still get the
    /// inline area they asked for. Zero means "uninitialized" and is
    /// treated as `1` by callers.
    pub(super) initial_viewport_height: u16,
    /// Latest height applied via `Terminal::set_viewport_height()`.
    /// Cached so the event loop can decide whether to call again (the
    /// underlying method is a no-op when current == new, but we'd
    /// rather avoid the call entirely on quiet frames).
    pub(super) current_viewport_height: u16,
    /// When the streaming high-water hold first saw a *large* gap between the
    /// viewport and what the live tail actually needs. `None` means no gap is
    /// currently open. See [`Self::streaming_shrink_release`].
    streaming_slack_since: Option<std::time::Instant>,
}

/// How many unused rows the streaming viewport has to be holding before the
/// high-water hold is allowed to let go. A tool card collapsing or a thinking
/// block draining frees a handful of rows and the next delta takes them right
/// back; a long-running command's tail (`cargo test`, `cargo build`) frees
/// dozens and never comes back for them.
const STREAMING_SHRINK_MIN_SLACK_ROWS: u16 = 8;

/// How long that gap has to stay open before the hold releases. Long enough
/// that a per-frame measurement dip — which the next streamed delta refills
/// within a frame or two — can never reach it.
const STREAMING_SHRINK_SLACK_HOLD: std::time::Duration = std::time::Duration::from_millis(600);

impl InlineRuntimeState {
    pub(super) fn with_initial_viewport_height(height: u16) -> Self {
        Self {
            initial_viewport_height: height,
            current_viewport_height: height,
            ..Default::default()
        }
    }

    /// Whether the streaming high-water hold should let the viewport shrink to
    /// `desired` this frame.
    ///
    /// The hold exists so a *transient* measurement dip never moves the prompt's
    /// bottom anchor (the "scroll down then repaint" wobble), and it deliberately
    /// keeps the viewport at its peak for the rest of the turn. That is right for
    /// the handful of rows a collapsing block frees, and wrong for the tens of rows
    /// a long command's output tail frees: the viewport stays stretched to
    /// nearly the whole screen while the live tail is three lines, so the turn
    /// runs to completion behind a screen-tall blank band.
    ///
    /// So the hold releases on a gap that is both large ([`STREAMING_SHRINK_MIN_SLACK_ROWS`])
    /// and durable ([`STREAMING_SHRINK_SLACK_HOLD`]) — the shape only the second
    /// case has. Any frame whose gap falls back under the row threshold rearms
    /// the timer, so a dip that the next delta refills never accumulates.
    pub(super) fn streaming_shrink_release(
        &mut self,
        desired_height: u16,
        now: std::time::Instant,
    ) -> bool {
        let slack = self.current_viewport_height.saturating_sub(desired_height);
        if slack < STREAMING_SHRINK_MIN_SLACK_ROWS {
            self.streaming_slack_since = None;
            return false;
        }
        let opened_at = *self.streaming_slack_since.get_or_insert(now);
        if now.duration_since(opened_at) < STREAMING_SHRINK_SLACK_HOLD {
            return false;
        }
        // Released: rearm, so the next stretch has to build its own gap rather
        // than inheriting this one's age and shrinking on the very next dip.
        self.streaming_slack_since = None;
        true
    }

    /// Drop any open slack timer. Called on every frame that is not a
    /// streaming-hold frame, so a gap only ages while it is actually being held.
    pub(super) fn clear_streaming_slack(&mut self) {
        self.streaming_slack_since = None;
    }

    pub(super) fn note_terminal_size(&mut self, size: (u16, u16)) -> bool {
        let resized = self
            .last_terminal_size
            .is_some_and(|previous| previous != size);
        self.last_terminal_size = Some(size);
        resized
    }

    /// Whether the terminal changed since the previous frame in a way that
    /// reflows the committed rows already pushed into real scrollback, so the
    /// inline arm must rebuild from our own source of truth instead of taking
    /// the incremental autoresize path.
    ///
    /// True ONLY on a WIDTH change. A width change makes the terminal re-wrap
    /// every committed scrollback row to the new width, mangling the startup
    /// banner's box-drawing borders into stray `┌───┌───` fragments. The
    /// incremental path only touches the live viewport, so it can never repair
    /// those — only a re-commit at the new width does.
    ///
    /// A height change — GROWTH or SHRINK — deliberately stays on the cheaper
    /// incremental path. Committed scrollback rows are fixed-width: the terminal
    /// does not reflow them when only the row count changes, so there is nothing
    /// above the viewport to repair. The live viewport itself follows the new
    /// height through `autoresize` + `set_viewport_height` on the incremental
    /// path, which already handles resizing onto a SHORTER screen. (An earlier
    /// version also reset on a height shrink, but that paid for an O(transcript)
    /// rebuild on a case the O(viewport) path already covers — pure cost, and on
    /// long transcripts a visible flash.)
    ///
    /// Compares against the size recorded by the previous frame's
    /// [`Self::note_terminal_size`]; returns false on the very first frame,
    /// before any size is known.
    pub(super) fn resize_needs_full_repaint(&self, size: (u16, u16)) -> bool {
        self.last_terminal_size
            .is_some_and(|(prev_w, _prev_h)| size.0 != prev_w)
    }
}

/// Maximum transcript length, in logical rows, for which a terminal *width*
/// change still triggers the full O(transcript) inline rebuild in
/// `inline_full_repaint_for_resize`. At or below it the rebuild reproduces every
/// committed row at the new width with full fidelity. ABOVE it the resize takes
/// the bounded light path instead — see that function's `# Budget` section for
/// why an unbounded rebuild must never run inside one synchronized-output frame.
///
/// Tunable via `REBON_INLINE_RESIZE_REPAINT_MAX_ROWS` (set `0` to always take
/// the light path; a large value to always rebuild). Note logical rows
/// UNDER-count physical lines: a wrapped row costs several screen lines, so the
/// real repaint cost can exceed what this row count suggests — keep the budget
/// conservative.
pub(super) fn inline_resize_repaint_budget() -> usize {
    const DEFAULT_MAX_ROWS: usize = 500;
    std::env::var("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_ROWS)
}

#[derive(Debug, Default, Clone)]
pub(super) struct InlineCommitCursor {
    committed_keys: Vec<String>,
    committed_row_count: usize,
    pub(super) last_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InlineCommitBatch {
    pub(super) start_row: usize,
    pub(super) end_row: usize,
}

impl InlineCommitCursor {
    pub(super) fn reset(&mut self) {
        self.committed_keys.clear();
        self.committed_row_count = 0;
        self.last_revision = 0;
    }

    /// Reconcile the cursor against the current transcript: keep the longest
    /// leading run of committed keys that still matches `rows` and forget the
    /// rest. Rows in that prefix are already in terminal scrollback and must
    /// NOT be re-emitted — a full `reset()` here would re-commit them and
    /// duplicate the scrollback (the prompt-withdraw bug). A wiped or
    /// replaced transcript degenerates to `reset()` (no keys match).
    pub(super) fn trim_to_rows(&mut self, rows: &[rebon_tui::Message]) {
        let check_len = rows.len().min(self.committed_keys.len());
        let mut matching = 0;
        for row in &rows[..check_len] {
            if inline_row_commit_key(row) == self.committed_keys[matching] {
                matching += 1;
            } else {
                break;
            }
        }
        self.committed_keys.truncate(matching);
        self.committed_row_count = matching;
        self.last_revision = 0;
    }

    #[cfg(test)]
    pub(super) fn prepare_batches(
        &mut self,
        rows: &[rebon_tui::Message],
        revision: u64,
    ) -> Vec<InlineCommitBatch> {
        self.prepare_batches_with_pinned_live_prefix(rows, revision, rows.len())
    }

    pub(super) fn prepare_batches_with_pinned_live_prefix(
        &mut self,
        rows: &[rebon_tui::Message],
        revision: u64,
        pinned_live_prefix: usize,
    ) -> Vec<InlineCommitBatch> {
        let max_committable = pinned_live_prefix.min(rows.len());
        if revision == self.last_revision
            && rows.len() >= self.committed_row_count
            && max_committable <= self.committed_row_count
        {
            return Vec::new();
        }
        let committed_prefix = match self.committed_prefix_len(rows) {
            Some(n) => n,
            None => {
                // The transcript no longer starts with the committed prefix
                // (e.g. ContextReset / auto-compaction cleared the transcript,
                // or it was truncated). Find how many leading rows still match
                // the committed keys — those are already in terminal scrollback
                // and must not be re-emitted. Trim the cursor to that point so
                // subsequent rows become committable again.
                let check_len = rows.len().min(self.committed_keys.len());
                let mut matching = 0;
                for i in 0..check_len {
                    if inline_row_commit_key(&rows[i]) == self.committed_keys[i] {
                        matching += 1;
                    } else {
                        break;
                    }
                }
                self.committed_keys.truncate(matching);
                matching
            }
        };
        self.committed_row_count = committed_prefix;
        self.last_revision = revision;
        if committed_prefix >= max_committable {
            return Vec::new();
        }
        vec![InlineCommitBatch {
            start_row: committed_prefix,
            end_row: max_committable,
        }]
    }

    fn committed_prefix_len(&self, rows: &[rebon_tui::Message]) -> Option<usize> {
        if self.committed_keys.is_empty() {
            return Some(0);
        }
        if rows.len() < self.committed_keys.len() {
            return None;
        }
        for (row_idx, committed_key) in self.committed_keys.iter().enumerate() {
            if inline_row_commit_key(&rows[row_idx]) != *committed_key {
                return None;
            }
        }
        Some(self.committed_keys.len())
    }

    pub(super) fn mark_committed(
        &mut self,
        batch: &InlineCommitBatch,
        rows: &[rebon_tui::Message],
        revision: u64,
    ) {
        let start = batch.start_row.min(rows.len());
        let end = batch.end_row.min(rows.len()).max(start);
        self.committed_keys
            .extend(rows[start..end].iter().map(inline_row_commit_key));
        self.committed_row_count = end;
        self.last_revision = revision;
    }

    pub(super) fn committed_row_count(&self) -> usize {
        self.committed_row_count
    }

    #[cfg(test)]
    pub(super) fn take_new<'a>(
        &mut self,
        rows: &'a [rebon_tui::Message],
    ) -> Vec<&'a rebon_tui::Message> {
        let revision = self.last_revision.saturating_add(1);
        let batches = self.prepare_batches(rows, revision);
        let mut out = Vec::new();
        for batch in batches {
            out.extend(rows[batch.start_row..batch.end_row].iter());
            self.mark_committed(&batch, rows, revision);
        }
        out
    }
}

fn inline_row_commit_key(row: &rebon_tui::Message) -> String {
    if let Some(uuid) = row.uuid() {
        format!("uuid:{uuid}")
    } else {
        let payload = serde_json::to_string(row).unwrap_or_else(|_| format!("{row:?}"));
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        format!("anon:{:x}", hasher.finalize())
    }
}

/// Drains an already-populated transcript directly into terminal scrollback
/// in a single `flush_inline_commits` pass, returning the resulting
/// `InlineRuntimeState` so the event loop can pick up where this left off
/// without re-committing any rows. The newline-based scroll path (with the
/// `scrolling-regions` feature disabled on ratatui) handles tall content
/// correctly via `scroll_up`, so we don't need to chunk message-by-message.
pub(super) fn prime_inline_scrollback_from_transcript(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut AppState,
    theme: &RenderTheme,
    initial_viewport_height: u16,
) -> anyhow::Result<InlineRuntimeState> {
    let mut runtime = InlineRuntimeState::with_initial_viewport_height(initial_viewport_height);
    let row_count = app.rebon_tui.transcript.len();
    if row_count == 0 {
        return Ok(runtime);
    }
    let terminal_area = terminal.size()?;
    let _ = runtime.note_terminal_size((terminal_area.width, terminal_area.height));
    app.transcript_measure_cache.clear();
    // Seed terminal size so the loop's first `flush_inline_commits` call
    // doesn't fire a resize synchronization for the geometry we just used.
    flush_inline_commits(terminal, app, theme, &mut runtime, row_count)?;
    Ok(runtime)
}

pub(super) fn measure_inline_commit_batches_for_prefix(
    app: &mut AppState,
    theme: &RenderTheme,
    runtime: &InlineRuntimeState,
    pinned_live_prefix: usize,
    width: u16,
) -> u16 {
    if width == 0 {
        return 0;
    }
    let rows = app.rebon_tui.transcript.rows();
    let revision = app.rebon_tui.transcript.revision();
    let mut cursor = runtime.commit_cursor.clone();
    let batches =
        cursor.prepare_batches_with_pinned_live_prefix(rows, revision, pinned_live_prefix);
    batches.iter().fold(0u16, |acc, batch| {
        let mut batch_cache = inline_commit_batch_cache();
        acc.saturating_add(measure_inline_commit_batch(
            app,
            theme,
            batch,
            width,
            &mut batch_cache,
        ))
    })
}

pub(super) fn flush_inline_commits<B: Backend>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut AppState,
    theme: &RenderTheme,
    runtime: &mut InlineRuntimeState,
    pinned_live_prefix: usize,
) -> anyhow::Result<()> {
    let rows = app.rebon_tui.transcript.rows();
    let revision = app.rebon_tui.transcript.revision();
    let terminal_area = terminal.size()?;
    let _ = runtime.note_terminal_size((terminal_area.width, terminal_area.height));
    let batches = runtime
        .commit_cursor
        .prepare_batches_with_pinned_live_prefix(rows, revision, pinned_live_prefix);
    if !batches.is_empty() {
        tracing::debug!(
            target: "inline_flush",
            total_rows = rows.len(),
            committed_before = runtime.commit_cursor.committed_row_count(),
            batch_count = batches.len(),
            overlay_blocks = app.rebon_tui.overlay.blocks.len(),
            pinned_live_prefix = pinned_live_prefix,
            "flush_inline_commits: committing batches to scrollback"
        );
    } else {
        tracing::trace!(
            target: "inline_flush",
            total_rows = rows.len(),
            committed = runtime.commit_cursor.committed_row_count(),
            overlay_blocks = app.rebon_tui.overlay.blocks.len(),
            pinned_live_prefix = pinned_live_prefix,
            "flush_inline_commits: no batches (nothing to commit this frame)"
        );
    }
    for batch in batches {
        let mut batch_cache = inline_commit_batch_cache();
        let height =
            measure_inline_commit_batch(app, theme, &batch, terminal_area.width, &mut batch_cache);
        if height == 0 {
            runtime
                .commit_cursor
                .mark_committed(&batch, app.rebon_tui.transcript.rows(), revision);
            continue;
        }
        let mut painted: u16 = 0;
        insert_before_cjk_safe(terminal, height, |buf| {
            painted =
                render_inline_commit_batch(app, theme, &batch, buf.area, buf, &mut batch_cache);
        })?;
        if painted != height {
            tracing::warn!(
                target: "inline_flush",
                start_row = batch.start_row,
                end_row = batch.end_row,
                measured = height,
                painted = painted,
                "inline commit batch: measured height differs from actual paint — under-measure CLIPS committed content out of scrollback (painted > measured); over-measure leaks trailing blank rows"
            );
        }
        runtime
            .commit_cursor
            .mark_committed(&batch, app.rebon_tui.transcript.rows(), revision);
    }
    Ok(())
}

pub(super) fn inline_commit_state_for_batch(
    app: &AppState,
    batch: &InlineCommitBatch,
) -> rebon_tui::AppState {
    let rows = app.rebon_tui.transcript.rows();
    let start = batch.start_row.min(rows.len());
    let end = batch.end_row.min(rows.len()).max(start);
    rebon_tui::AppState {
        transcript: rebon_tui::TranscriptStore::from_rows(rows[start..end].to_vec()),
        overlay: rebon_tui::StreamingOverlay::default(),
        flush_counter: app.rebon_tui.flush_counter,
    }
}

pub(super) fn inline_slice_needs_leading_segment_margin(row_count: usize, start: usize) -> bool {
    start > 0 && start < row_count
}

/// Whether `row` is the continuation half of an overflow-split text
/// block. Such a row must butt directly against the half already in
/// scrollback, so a batch starting with it gets no leading margin.
fn row_is_stream_continuation(row: &rebon_tui::Message) -> bool {
    matches!(
        row,
        rebon_tui::Message::Assistant(a) if a.is_stream_continuation == Some(true)
    )
}

fn inline_commit_verbosity(app: &AppState) -> rebon_tui::ToolOutputVerbosity {
    app.tool_output_verbosity
}

/// Extras for one commit batch.
///
/// The auto-mode annotation table rides along exactly as it does on the live
/// tail: a batch is painted once and its rows never come back, so a note
/// missing here is missing from scrollback for the rest of the session.
fn inline_commit_render_extras_for_batch<'a>(
    app: &'a AppState,
    batch: &InlineCommitBatch,
    rows: &[rebon_tui::Message],
) -> rebon_tui::TranscriptRenderExtras<'a> {
    let leading_segment_margin =
        inline_slice_needs_leading_segment_margin(rows.len(), batch.start_row)
            && !rows
                .get(batch.start_row)
                .is_some_and(row_is_stream_continuation);
    rebon_tui::TranscriptRenderExtras {
        leading_segment_margin,
        render_thinking_only_rows: true,
        force_verbose_edit_tool_previews: true,
        static_agent_group_status: true,
        auto_mode_allowed_tool_ids: &app.auto_mode_allowed_tool_ids,
        ..rebon_tui::TranscriptRenderExtras::empty()
    }
}

/// Measurement cache scoped to ONE commit batch, shared only between that
/// batch's measure and paint passes.
///
/// NEVER route these through the long-lived `app.transcript_measure_cache`:
/// every batch rebuilds its window store via `TranscriptStore::from_rows`,
/// whose transcript revision is a synthetic constant, so a cache that
/// outlives one batch satisfies the layout fast path with the PREVIOUS
/// batch's layout (same revision + render key, different rows). The
/// `insert_before` buffer is sized from the measurement — a stale height
/// clips the committed rows out of scrollback permanently (they can never be
/// re-emitted). Within a single batch the reuse is sound and load-bearing:
/// the paint pass hits the layout the measure pass built for the exact same
/// rows, so priming a long transcript doesn't measure everything twice.
/// Mirrors the fresh-cache rule the inline live paths follow (see the
/// comment in render/inline.rs).
pub(super) fn inline_commit_batch_cache() -> rebon_tui::TranscriptMeasureCache {
    rebon_tui::TranscriptMeasureCache::new()
}

pub(super) fn measure_inline_commit_batch(
    app: &mut AppState,
    theme: &RenderTheme,
    batch: &InlineCommitBatch,
    width: u16,
    batch_cache: &mut rebon_tui::TranscriptMeasureCache,
) -> u16 {
    if width == 0 || batch.start_row >= batch.end_row {
        return 0;
    }
    let state = inline_commit_state_for_batch(app, batch);
    let extras = inline_commit_render_extras_for_batch(app, batch, app.rebon_tui.transcript.rows());
    let area = Rect {
        x: 0,
        y: 0,
        width,
        height: 512,
    };
    let mut scratch = Buffer::empty(area);
    let result = rebon_tui::render_transcript_cached_with_running_hints(
        &state,
        area,
        &mut scratch,
        theme,
        usize::MAX,
        inline_commit_verbosity(app),
        0,
        None,
        batch_cache,
        false,
        extras,
    );
    result.total_lines.min(u16::MAX as usize) as u16
}

pub(super) fn render_inline_commit_batch(
    app: &mut AppState,
    theme: &RenderTheme,
    batch: &InlineCommitBatch,
    area: Rect,
    buf: &mut Buffer,
    batch_cache: &mut rebon_tui::TranscriptMeasureCache,
) -> u16 {
    let state = inline_commit_state_for_batch(app, batch);
    let extras = inline_commit_render_extras_for_batch(app, batch, app.rebon_tui.transcript.rows());
    let result = rebon_tui::render_transcript_cached_with_running_hints(
        &state,
        area,
        buf,
        theme,
        usize::MAX,
        inline_commit_verbosity(app),
        0,
        None,
        batch_cache,
        false,
        extras,
    );
    result.render_y_end.saturating_sub(area.y)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_config::UiMode;
    use std::time::Duration;

    use rebon_tui::{
        AssistantContentBlock, AssistantMessage, AssistantMessageInner, AssistantRole,
        AssistantTextBlock, StreamingContentBlock, ToolOutputVerbosity, UserContentBlock,
        UserMessage, UserMessageInner, UserRole, UserTextBlock,
    };
    use rebon_types::{
        ContentBlock, DiffContent, SessionUpdate, SessionUpdateParams, TextContent,
        ToolCallContent, ToolCallStatus, ToolKind,
    };

    fn user_message(uuid: &str, text: &str) -> rebon_tui::Message {
        rebon_tui::Message::User(UserMessage {
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

    fn assistant_message(uuid: &str, text: &str) -> rebon_tui::Message {
        rebon_tui::Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::Text(AssistantTextBlock {
                    text: text.into(),
                })],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn streaming_state_at(current_height: u16) -> InlineRuntimeState {
        InlineRuntimeState::with_initial_viewport_height(current_height)
    }

    #[test]
    fn streaming_hold_keeps_the_peak_through_a_small_dip() {
        // A collapsing tool card or a draining thinking block frees a few rows
        // and the next delta takes them back; shrinking for that is the
        // bottom-edge wobble the hold exists to prevent.
        let mut state = streaming_state_at(40);
        let start = std::time::Instant::now();

        assert!(!state.streaming_shrink_release(36, start));
        assert!(!state.streaming_shrink_release(36, start + Duration::from_secs(30)));
    }

    #[test]
    fn streaming_hold_keeps_the_peak_until_a_large_gap_is_durable() {
        let mut state = streaming_state_at(40);
        let start = std::time::Instant::now();

        assert!(!state.streaming_shrink_release(8, start));
        assert!(
            !state.streaming_shrink_release(
                8,
                start + STREAMING_SHRINK_SLACK_HOLD - Duration::from_millis(1)
            ),
            "a gap younger than the hold is still a candidate dip"
        );
        assert!(state.streaming_shrink_release(8, start + STREAMING_SHRINK_SLACK_HOLD));
    }

    #[test]
    fn streaming_hold_rearms_when_the_gap_closes() {
        // Regression: an aging gap must not be inherited. A long tail opens a
        // gap, the next delta refills it, and a later small dip would otherwise
        // read as "open for seconds already" and shrink immediately.
        let mut state = streaming_state_at(40);
        let start = std::time::Instant::now();

        assert!(!state.streaming_shrink_release(8, start));
        // Refilled: gap back under the row threshold.
        assert!(!state.streaming_shrink_release(38, start + Duration::from_millis(100)));
        // A new large gap starts its own clock.
        assert!(!state.streaming_shrink_release(8, start + Duration::from_millis(200)));
        assert!(state.streaming_shrink_release(
            8,
            start + Duration::from_millis(200) + STREAMING_SHRINK_SLACK_HOLD
        ));
    }

    #[test]
    fn streaming_hold_rearms_after_releasing() {
        let mut state = streaming_state_at(40);
        let start = std::time::Instant::now();

        assert!(!state.streaming_shrink_release(8, start));
        assert!(state.streaming_shrink_release(8, start + STREAMING_SHRINK_SLACK_HOLD));
        // The next stretch has to build its own gap rather than shrinking on
        // the very next frame.
        assert!(!state.streaming_shrink_release(8, start + STREAMING_SHRINK_SLACK_HOLD));
    }

    #[test]
    fn inline_commit_batches_request_static_agent_group_status() {
        let app = AppState::default();
        let rows = vec![user_message("u1", "hello")];
        let batch = InlineCommitBatch {
            start_row: 0,
            end_row: rows.len(),
        };

        let extras = inline_commit_render_extras_for_batch(&app, &batch, &rows);

        assert!(extras.static_agent_group_status);
    }

    #[test]
    fn committed_tool_card_keeps_its_auto_mode_note() {
        // Scrollback rows are painted once and never re-emitted, so a note
        // dropped from the commit paint is gone for the rest of the session.
        let mut app = AppState::default();
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(vec![assistant_tool(
            "a-read",
            "toolu-read",
            "Read",
            "src/lib.rs",
        )]);
        app.auto_mode_allowed_tool_ids.insert(
            "toolu-read".into(),
            rebon_types::AutoModeAllowSource::Classifier,
        );
        let batch = InlineCommitBatch {
            start_row: 0,
            end_row: app.rebon_tui.transcript.len(),
        };

        let mut batch_cache = inline_commit_batch_cache();
        let measured = measure_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            80,
            &mut batch_cache,
        );
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, measured));
        let painted = render_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            buf.area,
            &mut buf,
            &mut batch_cache,
        );
        let rendered = buffer_text(&buf);

        assert_eq!(painted, measured);
        assert!(
            rendered.contains("Allowed by auto mode classifier"),
            "{rendered:?}"
        );
    }

    fn assistant_tool(uuid: &str, id: &str, name: &str, path: &str) -> rebon_tui::Message {
        rebon_tui::Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(
                    rebon_tui::AssistantToolUseBlock {
                        id: id.into(),
                        name: name.into(),
                        input: serde_json::json!({ "path": path }),
                        tool_call_content: None,
                        raw_output: None,
                        title: Some(path.into()),
                        locations: None,
                        status: Some(ToolCallStatus::Completed),
                    },
                )],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn assistant_edit_tool(
        uuid: &str,
        id: &str,
        path: &str,
        new_text: String,
    ) -> rebon_tui::Message {
        rebon_tui::Message::Assistant(AssistantMessage {
            uuid: uuid.into(),
            timestamp: "t".into(),
            message: AssistantMessageInner {
                role: AssistantRole::Assistant,
                content: vec![AssistantContentBlock::ToolUse(
                    rebon_tui::AssistantToolUseBlock {
                        id: id.into(),
                        name: "Edit".into(),
                        input: serde_json::json!({ "file_path": path }),
                        tool_call_content: Some(vec![ToolCallContent::Diff(DiffContent {
                            path: path.into(),
                            old_text: Some(String::new()),
                            new_text,
                        })]),
                        raw_output: Some(serde_json::json!({ "type": "update" })),
                        title: Some(path.into()),
                        locations: None,
                        status: Some(ToolCallStatus::Completed),
                    },
                )],
            },
            is_api_error_message: None,
            advisor_model: None,
            is_stream_continuation: None,
        })
    }

    fn prepare_stable_batches(
        cursor: &mut InlineCommitCursor,
        rows: &[rebon_tui::Message],
        revision: u64,
    ) -> Vec<InlineCommitBatch> {
        cursor.prepare_batches(rows, revision)
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn inline_commit_batch_for_sliced_assistant_adds_one_leading_boundary_row() {
        let mut app = AppState::default();
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(vec![
            user_message("u1", "committed prompt"),
            assistant_message("a1", "later committed answer"),
        ]);
        let batch = InlineCommitBatch {
            start_row: 1,
            end_row: 2,
        };
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 4));

        let mut batch_cache = inline_commit_batch_cache();
        render_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            buf.area,
            &mut buf,
            &mut batch_cache,
        );
        let rendered = buffer_text(&buf);
        let mut lines = rendered.lines();

        assert_eq!(
            measure_inline_commit_batch(
                &mut app,
                &RenderTheme::plain(),
                &batch,
                80,
                &mut batch_cache
            ),
            2
        );
        assert!(
            lines.next().unwrap_or_default().trim().is_empty(),
            "{rendered}"
        );
        assert!(
            lines
                .next()
                .unwrap_or_default()
                .contains("later committed answer"),
            "{rendered}"
        );
    }

    #[test]
    fn inline_commit_batch_starting_on_continuation_row_drops_leading_boundary_row() {
        // The continuation half of an overflow-split text block must land
        // in scrollback flush against the half committed just above it —
        // the sliced-batch boundary row would reinsert the blank line the
        // split removed.
        let mut app = AppState::default();
        let mut continuation = assistant_message("a2", "continued half");
        if let rebon_tui::Message::Assistant(row) = &mut continuation {
            row.is_stream_continuation = Some(true);
        }
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(vec![
            assistant_message("a1", "first half"),
            continuation,
        ]);
        let batch = InlineCommitBatch {
            start_row: 1,
            end_row: 2,
        };

        let mut batch_cache = inline_commit_batch_cache();
        assert_eq!(
            measure_inline_commit_batch(
                &mut app,
                &RenderTheme::plain(),
                &batch,
                80,
                &mut batch_cache
            ),
            1,
            "no boundary row and no margin — the continuation is a single line"
        );
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 2));
        render_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            buf.area,
            &mut buf,
            &mut batch_cache,
        );
        let rendered = buffer_text(&buf);
        let first = rendered.lines().next().unwrap_or_default();
        assert!(first.contains("continued half"), "{rendered}");
        assert!(
            !first.contains('●'),
            "continuation rows must not repaint the gutter dot: {rendered}"
        );
    }

    #[test]
    fn continuation_tail_survives_two_line_full_height_scroll_commit() {
        use ratatui::{
            backend::TestBackend,
            text::Line,
            widgets::{Paragraph, Widget},
            Terminal, TerminalOptions, Viewport,
        };

        let mut app = AppState::default();
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText(
                "今后在 Bash 工具中，正确写法是：\n```sh".into(),
            ),
        );
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::FlushSealedPrefix {
                commit_timestamp: "t1".into(),
                policy: rebon_tui::SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
            },
        );
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("```sh")
        );
        let mut runtime = InlineRuntimeState::with_initial_viewport_height(10);
        let revision = app.rebon_tui.transcript.revision();
        let prefix_batch = runtime
            .commit_cursor
            .prepare_batches(app.rebon_tui.transcript.rows(), revision)
            .pop()
            .expect("prefix batch");
        runtime.commit_cursor.mark_committed(
            &prefix_batch,
            app.rebon_tui.transcript.rows(),
            revision,
        );

        let backend = TestBackend::new(88, 10);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(10),
            },
        )
        .expect("inline terminal constructs");
        insert_before_cjk_safe(&mut terminal, 2, |buf| {
            Paragraph::new(vec![Line::from("prefix-0"), Line::from("prefix-1")])
                .render(buf.area, buf);
        })
        .expect("seed two committed rows");
        assert_eq!(terminal.get_frame().area().top(), 0);
        assert_eq!(terminal.backend().scrollback().area.height, 2);

        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::AppendStreamingText(
                "\ncommand 2>/dev/null\n```\n\n只在明确使用 PowerShell 或 cmd.exe 时才应使用 NUL。"
                    .into(),
            ),
        );
        rebon_tui::reducer(
            &mut app.rebon_tui,
            rebon_tui::Action::FinalizeTurn {
                commit_uuid: "a-tail".into(),
                commit_timestamp: "t2".into(),
            },
        );
        assert!(matches!(
            app.rebon_tui.transcript.rows().get(1),
            Some(rebon_tui::Message::Assistant(row))
                if row.is_stream_continuation == Some(true)
        ));
        let committed_rows = app.rebon_tui.transcript.len();
        let expected_insert_height = measure_inline_commit_batches_for_prefix(
            &mut app,
            &RenderTheme::plain(),
            &runtime,
            committed_rows,
            88,
        );
        assert_eq!(expected_insert_height, 2);
        terminal
            .shrink_inline_viewport_keeping_top_for_insert_before(8, expected_insert_height)
            .expect("reserve two rows for the pending commit");

        flush_inline_commits(
            &mut terminal,
            &mut app,
            &RenderTheme::plain(),
            &mut runtime,
            committed_rows,
        )
        .expect("commit continuation tail");
        let visible_after_commit = buffer_text(terminal.backend().buffer());
        assert!(
            visible_after_commit.contains("command 2>/dev/null"),
            "{visible_after_commit}"
        );
        assert!(
            visible_after_commit.contains("PowerShell")
                && visible_after_commit.contains("cmd.exe")
                && visible_after_commit.contains("NUL。"),
            "{visible_after_commit}"
        );
        terminal
            .shrink_inline_viewport_keeping_top(4)
            .expect("shrink idle viewport after commit");
        terminal
            .draw(|frame| {
                Paragraph::new(vec![Line::from("prompt-0"), Line::from("prompt-1")])
                    .render(frame.area(), frame.buffer_mut());
            })
            .expect("draw live prompt after continuation commit");

        let visible_after_prompt_redraw = buffer_text(terminal.backend().buffer());
        assert!(
            visible_after_prompt_redraw.contains("command 2>/dev/null"),
            "{visible_after_prompt_redraw}"
        );
        assert!(
            visible_after_prompt_redraw.contains("PowerShell")
                && visible_after_prompt_redraw.contains("cmd.exe")
                && visible_after_prompt_redraw.contains("NUL。"),
            "{visible_after_prompt_redraw}"
        );
        let committed = format!(
            "{}\n{}",
            buffer_text(terminal.backend().scrollback()),
            visible_after_prompt_redraw
        );
        assert!(committed.contains("command 2>/dev/null"), "{committed}");
        assert!(
            committed.contains("PowerShell")
                && committed.contains("cmd.exe")
                && committed.contains("NUL。"),
            "{committed}"
        );
    }

    #[test]
    fn inline_commit_uses_live_verbosity_when_released() {
        let mut rows = Vec::new();
        for i in 0..320 {
            rows.push(assistant_tool(
                &format!("a-read-{i}"),
                &format!("toolu-read-{i}"),
                "Read",
                &format!("file_{i}.rs"),
            ));
        }
        let mut app = AppState::default();
        app.tool_output_verbosity = ToolOutputVerbosity::Verbose;
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(rows);
        let batch = InlineCommitBatch {
            start_row: 0,
            end_row: app.rebon_tui.transcript.len(),
        };

        let verbose_setting_height = measure_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            80,
            &mut inline_commit_batch_cache(),
        );
        app.tool_output_verbosity = ToolOutputVerbosity::Compact;
        let compact_setting_height = measure_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            80,
            &mut inline_commit_batch_cache(),
        );

        assert!(
            verbose_setting_height > compact_setting_height,
            "verbose release should preserve expanded tool output: verbose={verbose_setting_height}, compact={compact_setting_height}"
        );
        assert!(
            compact_setting_height < 500,
            "compact release should remain folded: {compact_setting_height}"
        );
    }

    #[test]
    fn inline_commit_preserves_large_verbose_edit_when_released() {
        let new_text = (0..700)
            .map(|i| format!("line_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = AppState::default();
        app.tool_output_verbosity = ToolOutputVerbosity::Verbose;
        app.rebon_tui.transcript =
            rebon_tui::TranscriptStore::from_rows(vec![assistant_edit_tool(
                "a-edit",
                "toolu-edit",
                "src/large.rs",
                new_text,
            )]);
        let batch = InlineCommitBatch {
            start_row: 0,
            end_row: app.rebon_tui.transcript.len(),
        };

        let mut batch_cache = inline_commit_batch_cache();
        let measured = measure_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            80,
            &mut batch_cache,
        );
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, measured));
        let painted = render_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            buf.area,
            &mut buf,
            &mut batch_cache,
        );
        let rendered = buffer_text(&buf);

        assert!(
            measured > 512,
            "verbose edit should be released in full: {measured}"
        );
        assert_eq!(painted, measured);
        assert!(!rendered.contains("lines hidden"), "{rendered:?}");
        assert!(
            rendered.contains("line_699"),
            "folded edit should preserve its tail: {rendered:?}"
        );
    }

    #[test]
    fn inline_commit_compact_long_edit_uses_prompt_fold() {
        let new_text = (1..=30)
            .map(|line| format!("change-{line:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = AppState::default();
        app.tool_output_verbosity = ToolOutputVerbosity::Compact;
        app.rebon_tui.transcript =
            rebon_tui::TranscriptStore::from_rows(vec![assistant_edit_tool(
                "a-edit",
                "toolu-edit",
                "src/long.rs",
                new_text,
            )]);
        let batch = InlineCommitBatch {
            start_row: 0,
            end_row: app.rebon_tui.transcript.len(),
        };

        let mut batch_cache = inline_commit_batch_cache();
        let measured = measure_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            80,
            &mut batch_cache,
        );
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, measured));
        let painted = render_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            buf.area,
            &mut buf,
            &mut batch_cache,
        );
        let rendered = buffer_text(&buf);

        assert_eq!(painted, measured);
        assert!(rendered.contains("(10 lines hidden)"), "{rendered:?}");
        assert!(rendered.contains("change-010"), "{rendered:?}");
        assert!(!rendered.contains("change-011"), "{rendered:?}");
        assert!(rendered.contains("change-021"), "{rendered:?}");
        assert!(rendered.contains("change-030"), "{rendered:?}");
    }

    #[test]
    fn insert_before_cjk_safe_full_height_commits_real_content_not_blank_rows() {
        use ratatui::{
            backend::TestBackend,
            text::Line,
            widgets::{Paragraph, Widget},
            Terminal, TerminalOptions, Viewport,
        };

        let backend = TestBackend::new(12, 4);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        assert_eq!(terminal.get_frame().area().y, 0);

        insert_before_cjk_safe(&mut terminal, 2, |buf| {
            Paragraph::new(vec![Line::from("A0"), Line::from("A1")]).render(buf.area, buf);
        })
        .expect("commit full-height content");

        let scrollback = terminal.backend().scrollback();
        assert_eq!(scrollback.area.height, 2);
        let row_text = |y: u16| -> String {
            (0..scrollback.area.width)
                .map(|x| scrollback.cell((x, y)).expect("cell in bounds").symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row_text(0), "A0");
        assert_eq!(row_text(1), "A1");
    }

    #[test]
    fn inline_commit_measure_and_paint_never_use_long_lived_cache() {
        // Companion to `inline_render_uses_fresh_cache_for_sliced_transcript`
        // in runner/render/mod.rs: commit batches rebuild their window store
        // via `TranscriptStore::from_rows` (synthetic constant revision), so
        // the long-lived AppState cache must never back their measure/paint —
        // it would satisfy the layout fast path with a previous batch's
        // layout and clip the committed rows out of scrollback.
        let source = include_str!("inline_commit_cursor.rs");
        let start = source
            .find("pub(super) fn measure_inline_commit_batches_for_prefix")
            .expect("measure_inline_commit_batches_for_prefix present");
        let end = start
            + source[start..]
                .find("#[cfg(test)]")
                .expect("test module present after the commit helpers");
        let body = &source[start..end];

        assert!(
            !body.contains("&mut app.transcript_measure_cache"),
            "inline commit measure/paint must not reuse AppState's long-lived measurement cache"
        );
        assert!(
            body.contains("inline_commit_batch_cache()"),
            "inline commit batches should allocate their per-batch cache via inline_commit_batch_cache()"
        );
    }

    #[test]
    fn flush_inline_commits_sizes_each_batch_from_its_own_rows() {
        // Regression for the "committed rows clipped to the previous batch's
        // height" bug: every commit batch rebuilds its window store via
        // `TranscriptStore::from_rows` (constant synthetic revision), so a
        // measurement cache shared ACROSS batches satisfied the layout fast
        // path with the previous batch's layout. The insert buffer was then
        // sized to that stale height (log fingerprint: "measured height
        // differs from actual paint measured=2 painted=14") and everything
        // past the first rows of each committed batch never reached
        // scrollback — tool result bodies and multi-line answers lost.
        use ratatui::{backend::TestBackend, Terminal, TerminalOptions, Viewport};

        let mut app = AppState::default();
        app.rebon_tui.transcript = rebon_tui::TranscriptStore::from_rows(vec![
            user_message("u1", "prompt"),
            assistant_message("a-short", "short reply"),
        ]);

        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )
        .expect("inline terminal constructs");
        let mut runtime = InlineRuntimeState::with_initial_viewport_height(4);
        let theme = RenderTheme::plain();

        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, 2)
            .expect("first flush commits the short batch");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 2);

        let long_text = (0..20)
            .map(|i| format!("line_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.rebon_tui
            .transcript
            .push(assistant_message("a-long", &long_text));
        flush_inline_commits(&mut terminal, &mut app, &theme, &mut runtime, 3)
            .expect("second flush commits the tall batch");
        assert_eq!(runtime.commit_cursor.committed_row_count(), 3);

        // The tall row's tail may still sit on-screen above the viewport, so
        // check scrollback and the visible screen together.
        let committed = format!(
            "{}\n{}",
            buffer_text(terminal.backend().scrollback()),
            buffer_text(terminal.backend().buffer())
        );
        assert!(
            committed.contains("line_19"),
            "tall batch must be committed at its own measured height, not the \
             previous batch's cached layout height: {committed}"
        );
    }

    #[test]
    fn mark_wide_shadow_cells_skip_marks_cjk_shadow_cells() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf.set_stringn(0, 0, "你好", 8, ratatui::style::Style::default());

        mark_wide_shadow_cells_skip(&mut buf);

        assert!(!buf[(0, 0)].skip);
        assert!(buf[(1, 0)].skip);
        assert!(!buf[(2, 0)].skip);
        assert!(buf[(3, 0)].skip);
    }

    #[test]
    fn inline_commit_cursor_commits_rows_once() {
        let rows = vec![user_message("u1", "hi")];
        let mut cursor = InlineCommitCursor::default();
        assert_eq!(cursor.take_new(&rows).len(), 1);
        assert!(cursor.take_new(&rows).is_empty());
    }

    #[test]
    fn inline_commit_cursor_reset_forgets_committed_rows_after_clear() {
        let rows = vec![user_message("u1", "before clear")];
        let mut cursor = InlineCommitCursor::default();
        let batch = cursor.prepare_batches(&rows, 1).pop().unwrap();
        cursor.mark_committed(&batch, &rows, 1);
        assert_eq!(cursor.committed_row_count(), 1);

        cursor.reset();

        assert_eq!(cursor.committed_row_count(), 0);
        assert_eq!(cursor.last_revision, 0);
        assert_eq!(cursor.prepare_batches(&rows, 2), vec![batch]);
    }

    #[test]
    fn trim_to_rows_after_withdraw_truncation_never_recommits_scrollback_prefix() {
        // Simulates the Esc prompt-withdraw flow: two rows already committed
        // to scrollback, a withdrawable prompt row appended (kept live), then
        // the withdraw truncates the transcript back to the first two rows.
        let committed_rows = vec![
            user_message("u1", "/model default"),
            assistant_message("a1", "Switched model for \"jun\" to \"default\"."),
        ];
        let mut cursor = InlineCommitCursor::default();
        let batch = cursor.prepare_batches(&committed_rows, 1).pop().unwrap();
        cursor.mark_committed(&batch, &committed_rows, 1);
        assert_eq!(cursor.committed_row_count(), 2);

        // Withdraw truncated the transcript back to the committed prefix.
        cursor.trim_to_rows(&committed_rows);

        // The prefix is already in terminal scrollback: re-preparing batches
        // must yield nothing (a full reset() would re-emit both rows).
        assert_eq!(cursor.committed_row_count(), 2);
        assert!(cursor.prepare_batches(&committed_rows, 2).is_empty());

        // New rows appended after the withdraw commit from index 2 onward.
        let mut extended = committed_rows.clone();
        extended.push(user_message("u2", "next prompt"));
        assert_eq!(
            cursor.prepare_batches(&extended, 3),
            vec![InlineCommitBatch {
                start_row: 2,
                end_row: 3,
            }]
        );
    }

    #[test]
    fn trim_to_rows_on_wiped_or_replaced_transcript_degenerates_to_reset() {
        let rows = vec![user_message("u1", "before")];
        let mut cursor = InlineCommitCursor::default();
        let batch = cursor.prepare_batches(&rows, 1).pop().unwrap();
        cursor.mark_committed(&batch, &rows, 1);

        // /new wipes the transcript entirely.
        cursor.trim_to_rows(&[]);
        assert_eq!(cursor.committed_row_count(), 0);

        // A resumed session replaces the rows wholesale: nothing matches, so
        // the whole new transcript becomes committable again.
        let replaced = vec![user_message("r1", "resumed row")];
        assert_eq!(
            cursor.prepare_batches(&replaced, 2),
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 1,
            }]
        );
    }

    #[test]
    fn resize_full_repaint_only_on_width_change() {
        let mut runtime = InlineRuntimeState::default();
        // First frame: no prior size recorded yet, so never a full repaint.
        assert!(!runtime.resize_needs_full_repaint((80, 24)));

        runtime.note_terminal_size((80, 24));
        // Unchanged geometry stays on the incremental path.
        assert!(!runtime.resize_needs_full_repaint((80, 24)));
        // A width change (either direction) reflows the committed scrollback
        // rows, so the inline arm must rebuild at the new width.
        assert!(runtime.resize_needs_full_repaint((100, 24)));
        assert!(runtime.resize_needs_full_repaint((60, 24)));
        // A height change at the SAME width never reflows fixed-width scrollback
        // rows, so BOTH a shrink and a growth stay on the cheaper incremental
        // path (there the live viewport follows the height via autoresize).
        assert!(!runtime.resize_needs_full_repaint((80, 20)));
        assert!(!runtime.resize_needs_full_repaint((80, 30)));
        // When the width ALSO changes, the width term forces a repaint no matter
        // which way the height moves.
        assert!(runtime.resize_needs_full_repaint((90, 20)));
        assert!(runtime.resize_needs_full_repaint((90, 30)));
        // `resize_needs_full_repaint` only reads the recorded size; it never
        // advances it, so the baseline stays at the seeded (80, 24).
        assert!(!runtime.resize_needs_full_repaint((80, 24)));
    }

    #[test]
    fn resize_repaint_budget_defaults_and_honors_env_override() {
        let _lock = crate::test_env::lock_env();
        let saved = std::env::var_os("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS");
        // SAFETY: env access is serialized by the `test_env` lock held above.
        unsafe { std::env::remove_var("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS") };
        assert_eq!(inline_resize_repaint_budget(), 500);
        unsafe { std::env::set_var("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS", "1200") };
        assert_eq!(inline_resize_repaint_budget(), 1200);
        // `0` is a valid "always take the light path" choice, not a parse error,
        // and surrounding whitespace is trimmed.
        unsafe { std::env::set_var("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS", "  0 ") };
        assert_eq!(inline_resize_repaint_budget(), 0);
        // Garbage falls back to the default instead of panicking.
        unsafe { std::env::set_var("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS", "not-a-number") };
        assert_eq!(inline_resize_repaint_budget(), 500);
        unsafe {
            match saved {
                Some(value) => std::env::set_var("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS", value),
                None => std::env::remove_var("REBON_INLINE_RESIZE_REPAINT_MAX_ROWS"),
            }
        }
    }

    #[test]
    fn inline_commit_cursor_prepares_append_only_rows_on_first_revision() {
        let rows = vec![user_message("u1", "hi")];
        let mut cursor = InlineCommitCursor::default();

        assert_eq!(
            cursor.prepare_batches(&rows, 1),
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 1
            }]
        );
        assert_eq!(cursor.committed_row_count(), 0);
    }

    #[test]
    fn inline_commit_cursor_shrinks_live_slice_after_marking_stable_rows() {
        let initial = vec![user_message("u1", "prompt")];
        let mut cursor = InlineCommitCursor::default();
        let initial_batch = cursor.prepare_batches(&initial, 1).pop().unwrap();
        cursor.mark_committed(&initial_batch, &initial, 1);
        assert_eq!(cursor.committed_row_count(), 1);

        let generated = vec![
            user_message("u1", "prompt"),
            assistant_message("a1", "stable answer line one"),
            assistant_message("a2", "stable answer line two"),
        ];
        let generated_batch = cursor.prepare_batches(&generated, 2).pop().unwrap();
        assert_eq!(generated_batch.start_row, 1);
        assert_eq!(generated_batch.end_row, 3);
        cursor.mark_committed(&generated_batch, &generated, 2);

        assert_eq!(cursor.committed_row_count(), generated.len());
        assert!(
            generated[cursor.committed_row_count().min(generated.len())..].is_empty(),
            "live slice should shrink after stable generated rows are committed"
        );
    }

    #[test]
    fn inline_flush_holds_completed_tool_during_streaming_thinking_for_grouping() {
        let mut app = AppState::default();
        app.ui_mode = UiMode::Inline;
        app.rebon_tui.transcript =
            rebon_tui::TranscriptStore::from_rows(vec![user_message("u1", "prompt")]);

        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::ToolCall {
                    tool_call_id: "toolu_search".into(),
                    title: "Search".into(),
                    kind: ToolKind::Search,
                    status: ToolCallStatus::Completed,
                    content: None,
                    locations: None,
                    raw_input: None,
                    raw_output: None,
                },
            },
        );
        assert_eq!(app.rebon_tui.transcript.rows().len(), 1);
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);

        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::ThinkingDelta {
                    text: "checking results".into(),
                },
            },
        );

        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(
            rows.len(),
            1,
            "thinking is transparent to tool grouping, so the tool should stay live"
        );
        assert_eq!(app.rebon_tui.overlay.blocks.len(), 2);
        assert!(matches!(
            &app.rebon_tui.overlay.blocks[1],
            StreamingContentBlock::Thinking(t) if t.thinking == "checking results" && t.is_streaming
        ));
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);

        let mut cursor = InlineCommitCursor::default();
        let initial_batch = cursor.prepare_batches(rows, 1).pop().unwrap();
        cursor.mark_committed(&initial_batch, rows, 1);
        assert!(cursor.prepare_batches(rows, 2).is_empty());
    }

    #[test]
    fn inline_visible_tool_splits_reasoning_while_hidden_tool_stays_transparent() {
        let mut app = AppState::default();
        app.ui_mode = UiMode::Inline;
        app.rebon_tui.transcript =
            rebon_tui::TranscriptStore::from_rows(vec![user_message("u1", "prompt")]);
        let send = |app: &mut AppState, update: SessionUpdate| {
            crate::tui::update::translate_session_update(
                app,
                SessionUpdateParams {
                    session_id: "s".into(),
                    update,
                },
            );
        };

        send(
            &mut app,
            SessionUpdate::ThinkingDelta {
                text: "Investigating getExistingWebIds usage".into(),
            },
        );
        send(&mut app, SessionUpdate::ThinkingEnd);
        assert_eq!(
            app.rebon_tui.transcript.len(),
            1,
            "unanchored thinking must stay live instead of entering immutable scrollback"
        );

        send(
            &mut app,
            SessionUpdate::ToolCall {
                tool_call_id: "read-1".into(),
                title: "Read webAPI.ts".into(),
                kind: ToolKind::Read,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(std::collections::HashMap::from([(
                    "file_path".into(),
                    serde_json::json!("src/helpers/webAPI.ts"),
                )])),
                raw_output: None,
            },
        );
        assert_eq!(
            app.rebon_tui.transcript.len(),
            2,
            "the visible Read boundary must release the preceding thinking"
        );
        send(
            &mut app,
            SessionUpdate::ToolCallUpdate {
                tool_call_id: "read-1".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            },
        );
        send(
            &mut app,
            SessionUpdate::ThinkingDelta {
                text: "Planning detailed error logging".into(),
            },
        );
        send(&mut app, SessionUpdate::ThinkingEnd);
        send(
            &mut app,
            SessionUpdate::ToolCall {
                tool_call_id: "hidden-search".into(),
                title: "ToolSearch".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Completed,
                content: None,
                locations: None,
                raw_input: Some(std::collections::HashMap::new()),
                raw_output: None,
            },
        );
        send(
            &mut app,
            SessionUpdate::ThinkingDelta {
                text: "Verifying public endpoint accessibility".into(),
            },
        );
        send(&mut app, SessionUpdate::ThinkingEnd);
        send(
            &mut app,
            SessionUpdate::ToolCall {
                tool_call_id: "invoke-1".into(),
                title: "InvokeDeferredTool".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Completed,
                content: None,
                locations: None,
                raw_input: Some(std::collections::HashMap::new()),
                raw_output: None,
            },
        );

        assert_eq!(app.rebon_tui.transcript.len(), 4);
        let mut live_buf = Buffer::empty(Rect::new(0, 0, 120, 20));
        let mut live_cache = rebon_tui::TranscriptMeasureCache::new();
        rebon_tui::render_transcript_cached_with_running_hints(
            &app.rebon_tui,
            live_buf.area,
            &mut live_buf,
            &RenderTheme::plain(),
            usize::MAX,
            ToolOutputVerbosity::Compact,
            0,
            None,
            &mut live_cache,
            false,
            rebon_tui::TranscriptRenderExtras {
                render_thinking_only_rows: true,
                ..rebon_tui::TranscriptRenderExtras::empty()
            },
        );
        let live = buffer_text(&live_buf);
        assert!(!live.contains("Reasoning (3 steps)"), "{live}");
        assert!(
            live.contains("· Investigating getExistingWebIds usage"),
            "{live}"
        );
        assert!(live.contains("Reasoning (2 steps)"), "{live}");
        assert!(live.contains("├ Planning detailed error logging"), "{live}");
        assert!(
            live.contains("└ Verifying public endpoint accessibility"),
            "{live}"
        );

        send(
            &mut app,
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "final answer".into(),
                    annotations: None,
                }),
            },
        );
        assert_eq!(app.rebon_tui.transcript.len(), 4);
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("final answer")
        );

        let batch = InlineCommitBatch {
            start_row: 1,
            end_row: app.rebon_tui.transcript.len(),
        };
        let mut committed_buf = Buffer::empty(Rect::new(0, 0, 120, 20));
        let mut batch_cache = inline_commit_batch_cache();
        render_inline_commit_batch(
            &mut app,
            &RenderTheme::plain(),
            &batch,
            committed_buf.area,
            &mut committed_buf,
            &mut batch_cache,
        );
        let committed = buffer_text(&committed_buf);
        assert!(!committed.contains("Reasoning (3 steps)"), "{committed}");
        assert!(
            committed.contains("· Investigating getExistingWebIds usage"),
            "{committed}"
        );
        assert!(committed.contains("Reasoning (2 steps)"), "{committed}");
        assert!(
            committed.contains("├ Planning detailed error logging"),
            "{committed}"
        );
        assert!(
            committed.contains("└ Verifying public endpoint accessibility"),
            "{committed}"
        );
    }

    #[test]
    fn inline_flush_drains_completed_output_before_large_live_tail_can_squeeze_it() {
        let mut app = AppState::default();
        app.ui_mode = UiMode::Inline;
        app.rebon_tui.transcript =
            rebon_tui::TranscriptStore::from_rows(vec![user_message("u1", "prompt")]);

        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::AgentMessageChunk {
                    content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                        text: "I will review without edits.".into(),
                        annotations: None,
                    }),
                },
            },
        );
        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::ToolCall {
                    tool_call_id: "toolu_search".into(),
                    title: "Search".into(),
                    kind: ToolKind::Search,
                    status: ToolCallStatus::Completed,
                    content: None,
                    locations: None,
                    raw_input: None,
                    raw_output: None,
                },
            },
        );
        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::AgentMessageChunk {
                    content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                        text: "Next stable summary after the tool.".into(),
                        annotations: None,
                    }),
                },
            },
        );

        let rows = app.rebon_tui.transcript.rows();
        // With live-tail text boundary: the completed tool drains immediately
        // once streaming text appears after it (the text IS the boundary).
        assert_eq!(
            rows.len(),
            3,
            "intro text + completed tool should commit once streaming text provides the assistant boundary"
        );
        assert_eq!(
            app.rebon_tui.overlay.blocks.len(),
            1,
            "only the streaming text should remain live"
        );
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 0);
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("Next stable summary after the tool.")
        );

        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::ToolCall {
                    tool_call_id: "toolu_next".into(),
                    title: "Bash".into(),
                    kind: ToolKind::Execute,
                    status: ToolCallStatus::InProgress,
                    content: None,
                    locations: None,
                    raw_input: None,
                    raw_output: None,
                },
            },
        );

        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(
            rows.len(),
            4,
            "tool cluster should commit once the following assistant text is sealed by the next tool boundary"
        );
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);

        let mut cursor = InlineCommitCursor::default();
        let initial_batch = cursor.prepare_batches(&rows[..1], 1).pop().unwrap();
        cursor.mark_committed(&initial_batch, &rows[..1], 1);
        let batches = cursor.prepare_batches(rows, 2);
        assert_eq!(
            batches,
            vec![InlineCommitBatch {
                start_row: 1,
                end_row: 4,
            }],
            "sealed assistant/tool/text rows should advance into scrollback together"
        );
        cursor.mark_committed(&batches[0], rows, 2);
        assert_eq!(cursor.committed_row_count(), 4);
    }

    #[test]
    fn force_drain_overflow_escape_hatch_evicts_held_tool_cluster_without_text_boundary() {
        // Sibling `inline_flush_holds_completed_tool_during_streaming_thinking_for_grouping`
        // pins this exact shape (completed tool + streaming thinking, no text
        // boundary) live under the *normal* flush so later tools can still join
        // the collapse group. That hold is unbounded, so when a tool-heavy turn
        // grows the overlay past even the terminal-height-capped viewport, the
        // bottom-anchored render clips the top of the held cluster and — since
        // overlay content is never `insert_before`'d — it never reaches
        // scrollback. `force_drain_overlay_sealed_prefix` is the overflow escape
        // hatch the event loop fires in exactly that case; it must therefore be
        // able to evict the held cluster even though no assistant text boundary
        // has closed the group.
        let mut app = AppState::default();
        app.ui_mode = UiMode::Inline;
        app.rebon_tui.transcript =
            rebon_tui::TranscriptStore::from_rows(vec![user_message("u1", "prompt")]);

        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::ToolCall {
                    tool_call_id: "toolu_search".into(),
                    title: "Search".into(),
                    kind: ToolKind::Search,
                    status: ToolCallStatus::Completed,
                    content: None,
                    locations: None,
                    raw_input: None,
                    raw_output: None,
                },
            },
        );
        crate::tui::update::translate_session_update(
            &mut app,
            rebon_types::SessionUpdateParams {
                session_id: "s".into(),
                update: rebon_types::SessionUpdate::ThinkingDelta {
                    text: "checking results".into(),
                },
            },
        );

        // Precondition: the normal hold policy keeps the completed tool live
        // (thinking is transparent to grouping, no text boundary yet).
        assert_eq!(
            app.rebon_tui.transcript.rows().len(),
            1,
            "normal flush holds the completed tool live (no assistant text boundary)"
        );
        assert_eq!(app.rebon_tui.overlay.blocks.len(), 2);
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);

        // Overflow escape hatch: must drain the held completed tool into the
        // transcript (→ committable to scrollback), leaving only the still
        // streaming thinking live.
        crate::tui::update::force_drain_overlay_sealed_prefix(&mut app);

        assert_eq!(
            app.rebon_tui.transcript.rows().len(),
            2,
            "force-drain evicts the held completed tool into the transcript"
        );
        assert_eq!(
            app.rebon_tui.overlay.blocks.len(),
            1,
            "only the still-streaming thinking should remain live after force-drain"
        );
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 0);
        assert!(matches!(
            &app.rebon_tui.overlay.blocks[0],
            StreamingContentBlock::Thinking(t) if t.thinking == "checking results" && t.is_streaming
        ));
    }

    #[test]
    fn inline_commit_cursor_keeps_withdrawable_prompt_rows_live() {
        let generated = vec![
            user_message("u1", "prompt"),
            assistant_message("a1", "stable answer"),
        ];
        let mut cursor = InlineCommitCursor::default();

        assert_eq!(
            cursor.prepare_batches_with_pinned_live_prefix(&generated, 2, 0),
            Vec::<InlineCommitBatch>::new()
        );
        assert_eq!(cursor.committed_row_count(), 0);
    }

    #[test]
    fn inline_commit_cursor_pins_later_queued_user_message_live() {
        let rows = vec![
            user_message("u1", "active prompt"),
            assistant_message("a1", "visible reply"),
            user_message("u2", "queued follow-up"),
        ];
        let mut cursor = InlineCommitCursor::default();

        let batches = cursor.prepare_batches_with_pinned_live_prefix(&rows, 1, 2);

        assert_eq!(
            batches,
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 2,
            }],
            "assistant output can commit, but queued user rows must remain live"
        );
    }

    #[test]
    fn inline_commit_cursor_commits_tool_rows_after_withdraw_pin_is_released() {
        let generated = vec![
            user_message("u1", "prompt"),
            assistant_message("edit_1", "Edit completed"),
            assistant_message("read_1", "Read completed"),
        ];
        let mut cursor = InlineCommitCursor::default();

        assert!(cursor
            .prepare_batches_with_pinned_live_prefix(&generated, 1, 0)
            .is_empty());
        assert_eq!(cursor.committed_row_count(), 0);

        let batches =
            cursor.prepare_batches_with_pinned_live_prefix(&generated, 2, generated.len());
        assert_eq!(
            batches,
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 3,
            }],
            "once visible reply/tool output exists, the withdrawn prompt pin must not keep stable tool rows live"
        );
    }

    #[test]
    fn inline_commit_cursor_resumes_after_pinned_prompt_becomes_committable() {
        let generated = vec![
            user_message("u1", "prompt"),
            assistant_message("a1", "stable answer"),
        ];
        let mut cursor = InlineCommitCursor::default();
        assert!(cursor
            .prepare_batches_with_pinned_live_prefix(&generated, 1, 0)
            .is_empty());

        let batches = cursor.prepare_batches(&generated, 2);
        assert_eq!(
            batches,
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 2,
            }]
        );
    }

    #[test]
    fn inline_commit_cursor_prepares_forward_only_batches_after_stable_observation() {
        let rows = vec![user_message("u1", "one"), user_message("u2", "two")];
        let mut cursor = InlineCommitCursor::default();
        assert_eq!(
            prepare_stable_batches(&mut cursor, &rows, 7),
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 2,
            }]
        );
        cursor.mark_committed(
            &InlineCommitBatch {
                start_row: 0,
                end_row: 2,
            },
            &rows,
            7,
        );
        assert!(cursor.prepare_batches(&rows, 8).is_empty());
        assert!(cursor.prepare_batches(&rows, 8).is_empty());

        let mut longer = rows.clone();
        longer.push(user_message("u3", "three"));
        longer.push(user_message("u4", "four"));
        assert_eq!(
            prepare_stable_batches(&mut cursor, &longer, 9),
            vec![InlineCommitBatch {
                start_row: 2,
                end_row: 4,
            }]
        );
        cursor.mark_committed(
            &InlineCommitBatch {
                start_row: 2,
                end_row: 4,
            },
            &longer,
            9,
        );
        assert_eq!(cursor.committed_row_count(), 4);
        assert_eq!(cursor.last_revision, 9);
    }

    #[test]
    fn inline_commit_cursor_recovers_from_prefix_insertion_before_any_commit() {
        let mut cursor = InlineCommitCursor::default();
        let early_visible = vec![user_message("u2", "later row visible first")];
        let first_batch = cursor.prepare_batches(&early_visible, 1).pop().unwrap();
        assert_eq!(first_batch.start_row, 0);
        assert_eq!(first_batch.end_row, 1);
        cursor.mark_committed(&first_batch, &early_visible, 1);
        assert_eq!(cursor.committed_row_count(), 1);

        // Rows rewritten with a different prefix — cursor resets and
        // re-offers all rows so they can be committed to scrollback.
        let final_order = vec![
            user_message("u1", "inserted before later row"),
            user_message("u2", "later row visible first"),
            user_message("u3", "tail"),
        ];
        assert_eq!(
            cursor.prepare_batches(&final_order, 2),
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 3,
            }]
        );
        assert_eq!(cursor.committed_row_count(), 0);
    }

    #[test]
    fn inline_commit_cursor_recovers_from_prefix_insertion_after_history_was_committed() {
        let original = vec![user_message("u1", "one"), user_message("u2", "two")];
        let mut cursor = InlineCommitCursor::default();
        let batch = prepare_stable_batches(&mut cursor, &original, 1)
            .pop()
            .unwrap();
        cursor.mark_committed(&batch, &original, 1);

        // Rows rewritten with a different prefix — cursor resets.
        let revised = vec![
            user_message("late", "inserted before committed rows"),
            user_message("u1", "one"),
            user_message("u2", "two"),
            user_message("u3", "three"),
        ];
        assert_eq!(
            cursor.prepare_batches(&revised, 2),
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 4,
            }]
        );
        assert_eq!(cursor.committed_row_count(), 0);
    }

    #[test]
    fn inline_commit_cursor_handles_anonymous_rows_without_index_shift_mismatch() {
        let original = vec![rebon_tui::Message::Unknown, user_message("u1", "one")];
        let mut cursor = InlineCommitCursor::default();
        let batch = prepare_stable_batches(&mut cursor, &original, 1)
            .pop()
            .unwrap();
        cursor.mark_committed(&batch, &original, 1);

        let appended = vec![
            rebon_tui::Message::Unknown,
            user_message("u1", "one"),
            user_message("u2", "two"),
        ];
        assert_eq!(
            cursor.prepare_batches(&appended, 2),
            vec![InlineCommitBatch {
                start_row: 2,
                end_row: 3
            }]
        );
    }

    #[test]
    fn inline_commit_cursor_handles_same_row_count_revision_without_duplicate() {
        let original = vec![user_message("u1", "one"), user_message("u2", "two")];
        let mut cursor = InlineCommitCursor::default();
        let batch = prepare_stable_batches(&mut cursor, &original, 1)
            .pop()
            .unwrap();
        cursor.mark_committed(&batch, &original, 1);

        let revised_same_count = vec![user_message("u1", "revised"), user_message("u2", "two")];
        assert!(cursor.prepare_batches(&revised_same_count, 2).is_empty());
        assert!(cursor.prepare_batches(&revised_same_count, 2).is_empty());
        assert_eq!(cursor.committed_row_count(), 2);
    }

    #[test]
    fn inline_commit_batch_state_excludes_overlay_and_uses_only_uncommitted_rows() {
        let mut app = AppState::new();
        app.rebon_tui
            .transcript
            .push(user_message("u1", "committed"));
        app.rebon_tui.transcript.push(user_message("u2", "new"));
        app.rebon_tui.overlay.set_streaming_text("live tail");

        let state = inline_commit_state_for_batch(
            &app,
            &InlineCommitBatch {
                start_row: 1,
                end_row: 2,
            },
        );
        assert_eq!(state.transcript.rows().len(), 1);
        assert_eq!(state.transcript.rows()[0].uuid(), Some("u2"));
        assert!(state.overlay.is_empty());
    }

    #[test]
    fn inline_commit_cursor_skips_rechecking_unchanged_revision() {
        let rows = vec![user_message("u1", "one"), user_message("u2", "two")];
        let mut cursor = InlineCommitCursor::default();
        let batch = prepare_stable_batches(&mut cursor, &rows, 1).pop().unwrap();
        cursor.mark_committed(&batch, &rows, 1);

        assert!(cursor.prepare_batches(&rows, 1).is_empty());
        assert_eq!(cursor.committed_row_count(), 2);
    }

    #[test]
    fn inline_commit_cursor_still_recovers_on_same_revision_truncation() {
        let rows = vec![user_message("u1", "one"), user_message("u2", "two")];
        let mut cursor = InlineCommitCursor::default();
        let batch = prepare_stable_batches(&mut cursor, &rows, 1).pop().unwrap();
        cursor.mark_committed(&batch, &rows, 1);

        let truncated = vec![user_message("u1", "one")];
        assert!(cursor.prepare_batches(&truncated, 1).is_empty());
        assert_eq!(cursor.committed_row_count(), 1);
    }

    #[test]
    fn inline_commit_cursor_recovers_after_transcript_clear() {
        // Simulates ContextReset / auto-compaction: transcript is cleared,
        // then new rows are added. The cursor must recover and commit the
        // new rows instead of permanently stalling.
        let original = vec![
            user_message("u1", "old prompt"),
            assistant_message("a1", "old answer"),
        ];
        let mut cursor = InlineCommitCursor::default();
        let batch = prepare_stable_batches(&mut cursor, &original, 1)
            .pop()
            .unwrap();
        cursor.mark_committed(&batch, &original, 1);
        assert_eq!(cursor.committed_row_count(), 2);

        // Transcript cleared (ContextReset), then repopulated with
        // entirely new messages.
        let post_reset = vec![
            user_message("u99", "new prompt after compaction"),
            assistant_message("a99", "new answer"),
        ];
        let batches = cursor.prepare_batches(&post_reset, 3);
        assert_eq!(
            batches,
            vec![InlineCommitBatch {
                start_row: 0,
                end_row: 2,
            }],
            "all post-reset rows should be committable"
        );
        assert_eq!(cursor.committed_row_count(), 0);

        // Mark them committed and verify normal operation resumes.
        cursor.mark_committed(&batches[0], &post_reset, 3);
        assert_eq!(cursor.committed_row_count(), 2);

        // Appending more rows works normally after recovery.
        let extended = vec![
            user_message("u99", "new prompt after compaction"),
            assistant_message("a99", "new answer"),
            user_message("u100", "follow-up"),
        ];
        assert_eq!(
            cursor.prepare_batches(&extended, 4),
            vec![InlineCommitBatch {
                start_row: 2,
                end_row: 3,
            }]
        );
    }

    #[test]
    fn inline_commit_cursor_recovers_after_truncation_below_committed_count() {
        let rows = vec![
            user_message("u1", "one"),
            user_message("u2", "two"),
            user_message("u3", "three"),
        ];
        let mut cursor = InlineCommitCursor::default();
        let batch = prepare_stable_batches(&mut cursor, &rows, 1).pop().unwrap();
        cursor.mark_committed(&batch, &rows, 1);
        assert_eq!(cursor.committed_row_count(), 3);

        // Transcript truncated to 1 row (fewer than committed count).
        // The remaining row still matches the committed prefix.
        let truncated = vec![user_message("u1", "one")];
        let batches = cursor.prepare_batches(&truncated, 2);
        assert!(
            batches.is_empty(),
            "u1 is already in scrollback, no new batch needed"
        );
        assert_eq!(cursor.committed_row_count(), 1);

        // New rows appended after truncation are committable.
        let extended = vec![
            user_message("u1", "one"),
            user_message("u4", "new row after truncation"),
        ];
        assert_eq!(
            cursor.prepare_batches(&extended, 3),
            vec![InlineCommitBatch {
                start_row: 1,
                end_row: 2,
            }]
        );
    }
}
