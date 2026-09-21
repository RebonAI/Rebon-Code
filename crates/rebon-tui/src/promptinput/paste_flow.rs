//! Paste planning and application.
//!
//! The caller still owns writing the results into its live state — the
//! pasted-content store, the inserted text, the cursor, the image cache, the
//! pending-space latch. This module only
//! computes the deterministic plan for text/image paste and orphaned image
//! cleanup.

use rebon_types::PromptPasteContent;

use crate::promptinput::input_modes::{get_mode_from_input, get_value_from_input, HistoryMode};
use crate::promptinput::input_paste::pasted_text_ref_num_lines;
use crate::promptinput::prompt_surface::parse_references;
use crate::promptinput::utils::clamp_cursor_offset;

/// Input bag for planning a text paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPasteInput {
    /// Raw pasted text before normalization.
    pub raw_text: String,
    /// Whether the current prompt input is empty.
    pub current_input_is_empty: bool,
    /// Next paste id to allocate when creating a text reference.
    pub next_paste_id: u32,
    /// Terminal row count.
    pub terminal_rows: i32,
}

/// Result of planning a text paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPastePlan {
    /// Pending-space-after-pill latch is always cleared before text paste.
    pub clear_pending_space_after_pill: bool,
    /// Optional mode switch derived from a leading `!` in an empty prompt.
    pub next_mode: Option<HistoryMode>,
    /// Text that should be inserted into the prompt.
    pub text_to_insert: String,
    /// Optional stored pasted content when we collapse to a reference.
    pub new_content: Option<PromptPasteContent>,
}

/// Input bag for planning an image paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePasteInput {
    /// Next paste id to allocate.
    pub next_paste_id: u32,
    /// Base64/inline payload.
    pub image: String,
    /// Optional media type.
    pub media_type: Option<String>,
    /// Optional filename.
    pub filename: Option<String>,
    /// Optional source path.
    pub source_path: Option<String>,
    /// Whether the previous image pill had already armed a lazy space.
    pub pending_space_after_pill: bool,
}

/// Result of planning an image paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePastePlan {
    /// Created image content row.
    pub new_content: PromptPasteContent,
    /// Prompt text that should be inserted.
    pub text_to_insert: String,
    /// Image paste always re-arms the lazy-space latch.
    pub arm_pending_space_after_pill: bool,
}

/// One event inside a "synthetic paste burst".
///
/// Terminals that don't deliver bracketed-paste (Windows conhost,
/// PuTTY with the setting off, older emulators) send a pasted block
/// as a rapid stream of individual key events instead of a single
/// `Event::Paste(String)`. The TUI layer detects this by polling
/// crossterm's event queue for zero-duration availability right
/// after reading a key; if more events are queued, it aggregates
/// the stream into a `Vec<PasteFragment>` via [`PasteBurstBuilder`]
/// and feeds the resulting text through [`apply_text_paste`].
///
/// Making this an enum (rather than a raw `char` stream) keeps
/// `Enter` and `Tab` distinguishable from plain characters, so the
/// pure logic knows how to translate them to `\n` / `\t` without
/// depending on crossterm types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteFragment {
    /// A printable character from the burst. Any char that isn't a
    /// control code the terminal handled specially (Enter/Tab).
    Char(char),
    /// A Return/Enter press. In a paste burst this is treated as a
    /// literal newline, never as a submit — the whole point of the
    /// burst detector is to prevent paste-newlines from triggering
    /// submit on terminals without bracketed-paste.
    Enter,
    /// A Tab press. Translated to a literal `\t`; the normalizer in
    /// [`plan_text_paste`] later expands it to four spaces, matching
    /// the paste handler's tab expansion.
    Tab,
}

impl PasteFragment {
    /// Append this fragment's text representation onto a buffer.
    ///
    /// The representation matches how a terminal would emit the key
    /// as a raw character: `Char(c)` → `c`, `Enter` → `'\n'`,
    /// `Tab` → `'\t'`. This keeps the produced string round-trip
    /// compatible with what `Event::Paste(String)` would have
    /// delivered on a bracketed-paste-capable terminal.
    pub fn push_into(&self, buf: &mut String) {
        match self {
            PasteFragment::Char(c) => buf.push(*c),
            PasteFragment::Enter => buf.push('\n'),
            PasteFragment::Tab => buf.push('\t'),
        }
    }
}

/// Stateful collector for a synthetic paste burst.
///
/// The TUI layer instantiates one of these when it detects that
/// more events are already queued immediately after reading a
/// key event, then pushes each pastable fragment via
/// [`PasteBurstBuilder::push`] until the burst ends (the queue
/// drains or a non-text key arrives). `count()` and
/// `contains_newline()` let the caller decide whether the
/// aggregated burst is large enough or newline-bearing enough to
/// warrant routing through [`apply_text_paste`] instead of being
/// processed as individual key events.
///
/// The builder is newline-tracking from the start: even a
/// two-fragment burst that happens to contain an Enter must be
/// treated as a paste, because that Enter would otherwise trip
/// the normal Return → Submit path — which is exactly the
/// "the newlines were sent straight through as Enter" bug this module fixes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PasteBurstBuilder {
    text: String,
    count: usize,
    has_newline: bool,
}

impl PasteBurstBuilder {
    /// Start a new empty burst.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a fragment to the running burst.
    pub fn push(&mut self, fragment: PasteFragment) {
        if matches!(fragment, PasteFragment::Enter) {
            self.has_newline = true;
        }
        fragment.push_into(&mut self.text);
        self.count += 1;
    }

    /// Number of fragments pushed so far.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Whether at least one `PasteFragment::Enter` has been pushed.
    /// This is the load-bearing signal that flips a burst into
    /// "must be treated as a paste" territory.
    pub fn contains_newline(&self) -> bool {
        self.has_newline
    }

    /// Borrow the accumulated raw text without consuming the
    /// builder. Useful for testing or for feeding the builder's
    /// text into [`apply_text_paste`] while still querying
    /// `count()` / `contains_newline()` after the call.
    pub fn as_text(&self) -> &str {
        &self.text
    }

    /// Consume the builder and return the accumulated raw text.
    pub fn into_text(self) -> String {
        self.text
    }
}

/// Should the caller apply the accumulated burst via
/// [`apply_text_paste`]?
///
/// Returns `true` iff the burst has **at least two** fragments.
/// A single-fragment "burst" is indistinguishable from a single
/// keystroke and must never be treated as a paste — otherwise a
/// stray Press Enter that leaks into the event queue on startup
/// (e.g. the shell's Enter that launched rebon) gets applied as
/// a `"\n"` paste and the input opens with a mysterious blank
/// second line. Genuine pastes always deliver ≥ 2 fragments
/// (even a pasted newline sequence comes as at least one char
/// followed by Enter), so the `count >= 2` floor is safe.
///
/// The caller is responsible for filtering out Release events
/// BEFORE they reach this check — a Windows Press+Release pair
/// for one real keystroke should not count as two fragments.
/// The TUI layer does this by skipping `KeyEventKind::Release`
/// events during burst aggregation without terminating the burst.
pub fn should_apply_paste_burst(builder: &PasteBurstBuilder) -> bool {
    builder.count() >= 2
}

/// Everything the caller needs to feed in to apply a text paste to
/// a prompt-input buffer. Bundles the raw pasted text together with
/// the live input/cursor/paste-store snapshot so the function can
/// own the full splice + id-allocation dance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyTextPasteState {
    /// Raw pasted text before normalization.
    pub raw_text: String,
    /// Current prompt input buffer.
    pub current_input: String,
    /// Current cursor byte offset into `current_input`.
    pub cursor_offset: usize,
    /// Current stored pasted-content payloads.
    pub pasted_contents: Vec<PromptPasteContent>,
    /// Next paste id to allocate.
    pub next_paste_id: u32,
    /// Terminal row count.
    pub terminal_rows: i32,
}

/// Result of applying a text paste — all the mutated slots the
/// caller should write back into its live state. Ownership-free
/// clone of the post-paste state bag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyTextPasteResult {
    /// New prompt input buffer (with either the raw text spliced in
    /// at the cursor, or a `[Pasted text #N]` reference chip).
    pub input: String,
    /// New cursor byte offset into `input`.
    pub cursor_offset: usize,
    /// New stored pasted-content payloads.
    pub pasted_contents: Vec<PromptPasteContent>,
    /// Next paste id (incremented iff a new reference was created).
    pub next_paste_id: u32,
    /// Optional mode switch when the pasted text began with an input
    /// mode marker in an empty prompt (e.g. `!` → Bash mode).
    pub next_mode: Option<HistoryMode>,
}

/// Apply a text paste end-to-end.
///
/// This is the higher-level sibling of [`plan_text_paste`] that owns
/// the full apply dance the TUI caller previously did inline: run
/// the paste plan, splice the resulting text at the cursor, advance
/// the cursor past the inserted text, bump `next_paste_id` if a new
/// reference chip was created, and append any new
/// [`PromptPasteContent`] row to the store.
///
/// Keeping this in the state-machine `promptinput` module means
/// the TUI shell only needs to hand it a snapshot of the relevant
/// state and write back the result — the splice math and id
/// allocation live next to `plan_text_paste` where they can be
/// unit-tested without a terminal.
pub fn apply_text_paste(state: ApplyTextPasteState) -> ApplyTextPasteResult {
    let plan = plan_text_paste(&TextPasteInput {
        raw_text: state.raw_text,
        current_input_is_empty: state.current_input.is_empty(),
        next_paste_id: state.next_paste_id,
        terminal_rows: state.terminal_rows,
    });

    let mut pasted_contents = state.pasted_contents;
    let mut next_paste_id = state.next_paste_id;
    if let Some(content) = plan.new_content {
        next_paste_id = content.id + 1;
        pasted_contents.push(content);
    }

    let cursor = clamp_cursor_offset(&state.current_input, state.cursor_offset);
    let before = &state.current_input[..cursor];
    let after = &state.current_input[cursor..];
    let input = format!("{before}{}{after}", plan.text_to_insert);
    let cursor_offset = cursor + plan.text_to_insert.len();

    ApplyTextPasteResult {
        input,
        cursor_offset,
        pasted_contents,
        next_paste_id,
        next_mode: plan.next_mode,
    }
}

/// Input for [`try_merge_into_prev_text_paste`].
///
/// Borrowed snapshot of the relevant app state — no ownership is
/// transferred unless the caller accepts the returned
/// [`MergedPasteResult`] and writes it back.
#[derive(Debug, Clone, Copy)]
pub struct TryMergePasteInput<'a> {
    /// Raw pasted text for the new burst. Will be normalized the same
    /// way [`plan_text_paste`] does before being appended.
    pub raw_text: &'a str,
    /// Current prompt input buffer.
    pub current_input: &'a str,
    /// Current cursor byte offset into `current_input`.
    pub cursor_offset: usize,
    /// Current stored pasted-content payloads.
    pub pasted_contents: &'a [PromptPasteContent],
}

/// Successful merge result — the caller should write these back
/// into its live state. Only the chip whose id is
/// `updated_content_id` has changed in `pasted_contents`; replace
/// that row's `content` with `updated_content_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPasteResult {
    /// New prompt input buffer.
    pub input: String,
    /// New cursor byte offset.
    pub cursor_offset: usize,
    /// Id of the chip whose content was extended.
    pub updated_content_id: u32,
    /// The combined content (previous chip content + normalized new text).
    pub updated_content_text: String,
    /// Bytes of stray non-ASCII tail absorbed from between the chip
    /// ref and end of prompt. Zero on the exact-match path, non-zero
    /// when the fallback absorbed a conpty-straggler char. Exposed so
    /// the TUI caller can log when the heuristic fired — if a user
    /// reports "my CJK char was swallowed", the logs will show it.
    pub absorbed_stray_tail_bytes: usize,
}

/// Why [`try_merge_into_prev_text_paste`] declined to merge.
///
/// Surfaced through [`try_merge_into_prev_text_paste_diag`] so callers
/// can log the failure reason and feed it back into debugging the
/// "split paste produced N chips" bug. The `Option`-returning helper
/// collapses these into `None` for callers that just need a yes/no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRejectReason {
    /// `pasted_contents` has no `"text"` chip to merge into.
    NoTextChip,
    /// The prompt no longer ends with the previous chip's reference.
    /// The user typed or edited characters after the chip, so merging
    /// would step on their input. Includes the expected ref and the
    /// actual tail for diagnostics.
    PromptTailMismatch {
        /// The chip reference string the prompt was expected to end with
        /// (e.g. `"[Pasted text #1 +11 lines]"`).
        expected_ref: String,
        /// Last up-to-`expected_ref.len()` bytes of the prompt so logs
        /// can show what landed there instead.
        actual_tail: String,
    },
    /// Cursor is not at the very end of the prompt — user has
    /// navigated elsewhere, so appending at end would surprise them.
    CursorNotAtEnd {
        /// Current cursor byte offset into the prompt.
        cursor_offset: usize,
        /// Total byte length of the prompt buffer.
        input_len: usize,
    },
}

impl MergeRejectReason {
    /// Stable machine-readable label for tracing merge rejection reasons.
    pub fn diagnostic_name(&self) -> &'static str {
        match self {
            Self::NoTextChip => "no_text_chip",
            Self::PromptTailMismatch { .. } => "prompt_tail_mismatch",
            Self::CursorNotAtEnd { .. } => "cursor_not_at_end",
        }
    }
}

/// Attempt to merge a new burst-flushed paste into the most recent
/// text chip instead of creating a new one.
///
/// Non-bracketed paste on Windows can be split across two
/// `flush_burst_as_paste` calls when conpty batches the pasted events
/// with a gap larger than `BURST_IDLE_TIMEOUT` (150 ms). Without this
/// helper, the user sees one logical paste rendered as two adjacent
/// chips (`[Pasted text #1 +11 lines][Pasted text #2 +1 lines]`). The
/// TUI runner tracks the timestamp of the last flush and, when a
/// second flush lands within its merge grace window, calls this
/// helper to coalesce.
///
/// Returns `Some(result)` iff:
/// * the latest `"text"` chip in `pasted_contents` has its reference
///   sitting at the end of `current_input`, **and**
/// * `cursor_offset` is at the end of `current_input` (the user
///   hasn't typed or cursor-moved between the two flushes).
///
/// Returns `None` otherwise — the caller should fall back to the
/// normal `apply_text_paste` path.
pub fn try_merge_into_prev_text_paste(input: TryMergePasteInput<'_>) -> Option<MergedPasteResult> {
    try_merge_into_prev_text_paste_diag(input).ok()
}

/// Diagnostic variant of [`try_merge_into_prev_text_paste`] that returns
/// a [`MergeRejectReason`] instead of `None` so the caller can log the
/// specific guard that failed. Used by the TUI runner to trace why a
/// chip-merge attempt was rejected (the root-cause for bugs where a
/// single paste surfaces as multiple adjacent chips).
pub fn try_merge_into_prev_text_paste_diag(
    input: TryMergePasteInput<'_>,
) -> Result<MergedPasteResult, MergeRejectReason> {
    let last = input
        .pasted_contents
        .iter()
        .rev()
        .find(|c| c.kind == "text")
        .ok_or(MergeRejectReason::NoTextChip)?;
    let current_lines = pasted_text_ref_num_lines(&last.content);
    let current_ref = format_pasted_text_ref(last.id, current_lines);

    // Primary path: prompt ends with the chip ref exactly.
    // Fallback path: prompt ends with `chip_ref + <short, single-line
    // extra>`. conpty can split a bracketed paste at non-ASCII byte
    // boundaries and deliver the straddling character as a raw Key
    // event that lands between the chip ref and the next
    // Event::Paste chunk — so the input may contain a stray "—" (or
    // similar) between the chip ref and end of input when the next
    // chunk arrives. The fallback absorbs that stray tail into the
    // chip content so the logical paste stays a single chip.
    let (prefix_len, stray_tail) = if input.current_input.ends_with(&current_ref) {
        (input.current_input.len() - current_ref.len(), "")
    } else if let Some((prefix, extra)) =
        extract_prefix_and_stray_tail(input.current_input, &current_ref, MAX_STRAY_TAIL_BYTES)
    {
        (prefix.len(), extra)
    } else {
        let tail_start = input
            .current_input
            .len()
            .saturating_sub(current_ref.len().saturating_add(16));
        // Snap to a char boundary so the diagnostic slice never
        // panics on multibyte content.
        let mut snap = tail_start;
        while snap < input.current_input.len() && !input.current_input.is_char_boundary(snap) {
            snap += 1;
        }
        return Err(MergeRejectReason::PromptTailMismatch {
            expected_ref: current_ref,
            actual_tail: input.current_input[snap..].to_string(),
        });
    };

    if input.cursor_offset != input.current_input.len() {
        return Err(MergeRejectReason::CursorNotAtEnd {
            cursor_offset: input.cursor_offset,
            input_len: input.current_input.len(),
        });
    }

    // Fold the stray tail (if any) into the chip content BEFORE the
    // newly-pasted body, preserving the order the user sees.
    let combined_content = format!(
        "{}{}{}",
        last.content,
        normalize_pasted_text(stray_tail),
        normalize_pasted_text(input.raw_text)
    );
    let new_lines = pasted_text_ref_num_lines(&combined_content);
    let new_ref = format_pasted_text_ref(last.id, new_lines);
    let mut new_input = String::with_capacity(prefix_len + new_ref.len());
    new_input.push_str(&input.current_input[..prefix_len]);
    new_input.push_str(&new_ref);
    let cursor_offset = new_input.len();
    Ok(MergedPasteResult {
        input: new_input,
        cursor_offset,
        updated_content_id: last.id,
        updated_content_text: combined_content,
        absorbed_stray_tail_bytes: stray_tail.len(),
    })
}

/// Input for [`repair_overadopted_text_chip`].
///
/// Borrowed snapshot of the relevant app state — nothing is mutated
/// unless the caller accepts the returned result and writes it back.
#[derive(Debug, Clone, Copy)]
pub struct OveradoptedChipRepairInput<'a> {
    /// Current prompt input buffer.
    pub current_input: &'a str,
    /// Current cursor byte offset into `current_input`.
    pub cursor_offset: usize,
    /// Current stored pasted-content payloads.
    pub pasted_contents: &'a [PromptPasteContent],
    /// Id of the chip whose content was optimistically adopted.
    pub chip_id: u32,
    /// The full normalized text that was adopted from the clipboard.
    /// Must be a suffix of the chip's stored content (the chip may
    /// additionally hold earlier merged content before it).
    pub adopted_norm: &'a str,
    /// Bytes of `adopted_norm` the terminal stream actually confirmed.
    /// Must sit on a char boundary of `adopted_norm`.
    pub confirmed_len: usize,
}

/// Result of [`repair_overadopted_text_chip`] — write these back into
/// live state. Only the chip with the input's `chip_id` changed in
/// `pasted_contents`; replace its `content` with
/// `updated_content_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OveradoptedChipRepairResult {
    /// New prompt input buffer (chip reference re-labelled with the
    /// corrected line count).
    pub input: String,
    /// New cursor byte offset into `input`.
    pub cursor_offset: usize,
    /// The trimmed chip content.
    pub updated_content_text: String,
    /// How many bytes were trimmed off the chip content.
    pub trimmed_bytes: usize,
}

/// Shrink a text chip whose content was optimistically adopted from
/// the OS clipboard when the terminal's paste stream turned out to be
/// shorter than the clipboard (e.g. an X11 primary-selection paste
/// that happens to be a prefix of the clipboard). The unconfirmed
/// suffix of the adopted text is removed from the chip content and
/// the `[Pasted text #N +X lines]` reference in the prompt is
/// re-labelled with the corrected line count.
///
/// Returns `None` when there is nothing to repair: the stream
/// confirmed the full adoption, the chip no longer exists, or the
/// chip content does not end with `adopted_norm` (something else
/// already rewrote it — leave it alone).
pub fn repair_overadopted_text_chip(
    input: OveradoptedChipRepairInput<'_>,
) -> Option<OveradoptedChipRepairResult> {
    if input.confirmed_len >= input.adopted_norm.len() {
        return None;
    }
    let chip = input
        .pasted_contents
        .iter()
        .find(|c| c.id == input.chip_id && c.kind == "text")?;
    if !chip.content.ends_with(input.adopted_norm) {
        return None;
    }
    // Snap the confirmed length to a char boundary so slicing the
    // adopted text can never panic on multibyte content.
    let mut confirmed = input.confirmed_len.min(input.adopted_norm.len());
    while confirmed > 0 && !input.adopted_norm.is_char_boundary(confirmed) {
        confirmed -= 1;
    }
    let keep = chip.content.len() - input.adopted_norm.len() + confirmed;
    let updated_content_text = chip.content[..keep].to_string();
    let trimmed_bytes = chip.content.len() - keep;

    let old_ref = format_pasted_text_ref(chip.id, pasted_text_ref_num_lines(&chip.content));
    let new_ref = format_pasted_text_ref(chip.id, pasted_text_ref_num_lines(&updated_content_text));

    let (new_input, cursor_offset) = if input.current_input.ends_with(&old_ref) {
        let prefix_len = input.current_input.len() - old_ref.len();
        let mut s = String::with_capacity(prefix_len + new_ref.len());
        s.push_str(&input.current_input[..prefix_len]);
        s.push_str(&new_ref);
        let cursor = if input.cursor_offset >= input.current_input.len() {
            s.len()
        } else {
            clamp_cursor_offset(input.current_input, input.cursor_offset).min(prefix_len)
        };
        (s, cursor)
    } else {
        // Prompt tail changed since adoption — keep the input as-is
        // (content correctness still matters; the label is best-effort).
        (
            input.current_input.to_string(),
            clamp_cursor_offset(input.current_input, input.cursor_offset),
        )
    };

    Some(OveradoptedChipRepairResult {
        input: new_input,
        cursor_offset,
        updated_content_text,
        trimmed_bytes,
    })
}

/// Largest "stray tail" (bytes after the chip ref) the merge fallback
/// will absorb into the chip content. Measured in **bytes**, not
/// graphemes or codepoints — 48 bytes comfortably covers the observed
/// short Windows suffix pollution (`· ∴`, `Thinking…`) while still
/// staying small enough to avoid absorbing a meaningful user-authored
/// continuation.
const MAX_STRAY_TAIL_BYTES: usize = 48;

/// Largest ASCII-bearing suffix the merge fallback will tolerate when
/// it matches the specific short UI-ish pollution pattern observed in
/// Windows raw-key paste splits.
const MAX_UI_STRAY_TAIL_BYTES: usize = 24;

/// If `input` ends with `ref_text` followed by a short, newline-free,
/// tolerated extra tail, return `(prefix_before_ref, extra_tail)`.
/// Otherwise return `None` so the caller falls through to its mismatch
/// path.
///
/// Primary tolerance is still biased toward non-ASCII stragglers — the
/// original Windows/conpty failure mode was a split multibyte codepoint.
/// But the raw-key burst diagnostics also showed a second class of
/// suffix pollution after the chip ref: tiny UI-ish tails like `· ∴`
/// and `Thinking…`. Those are not deliberate prompt edits; they are
/// transient prompt-suffix noise that should not force a new paste chip.
/// We therefore also tolerate a very small, highly-constrained class of
/// ASCII-bearing tails that are short, single-line, and composed only of
/// punctuation/whitespace plus an optional `Thinking…` token.
///
/// Anything broader — long text, alphanumeric words other than the
/// explicitly allowed token, embedded newlines, etc. — still rejects so
/// deliberate user typing remains outside the merge heuristic.
fn extract_prefix_and_stray_tail<'a>(
    input: &'a str,
    ref_text: &str,
    max_stray_bytes: usize,
) -> Option<(&'a str, &'a str)> {
    let tail_start = input.rfind(ref_text)?;
    let after_ref = &input[tail_start + ref_text.len()..];
    if after_ref.is_empty() {
        return None; // exact match — primary path handles this.
    }
    if after_ref.len() > max_stray_bytes {
        return None;
    }
    if after_ref.contains('\n') || after_ref.contains('\r') {
        return None;
    }
    if stray_tail_is_mergeable(after_ref) {
        Some((&input[..tail_start], after_ref))
    } else {
        None
    }
}

fn stray_tail_is_mergeable(after_ref: &str) -> bool {
    if after_ref.chars().all(|c| !c.is_ascii()) {
        return true;
    }

    if after_ref.len() > MAX_UI_STRAY_TAIL_BYTES {
        return false;
    }

    let trimmed = after_ref.trim();
    if trimmed.is_empty() {
        return false;
    }

    let thinking_marker = "Thinking…";
    let mut remainder = trimmed;
    if let Some(stripped) = remainder.strip_prefix(thinking_marker) {
        remainder = stripped.trim();
        return remainder.is_empty() || remainder.chars().all(is_allowed_ui_suffix_char);
    }

    trimmed.chars().all(is_allowed_ui_suffix_char)
}

fn is_allowed_ui_suffix_char(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '·' | '∴'
                | '•'
                | '…'
                | '.'
                | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | '-'
                | '—'
                | '_'
                | '/'
                | '\\'
                | '|'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '\''
                | '"'
        )
}

/// Ids of stored image rows that the prompt text no longer references.
pub fn prune_orphaned_image_ids(input: &str, pasted_contents: &[PromptPasteContent]) -> Vec<u32> {
    let referenced_ids = parse_references(input)
        .into_iter()
        .map(|matched| matched.id)
        .collect::<std::collections::BTreeSet<_>>();
    pasted_contents
        .iter()
        .filter(|content| content.kind == "image" && !referenced_ids.contains(&content.id))
        .map(|content| content.id)
        .collect()
}

/// Plan a text paste: normalize the raw text, decide whether a leading mode
/// marker should switch prompt mode, and either splice the text inline or
/// collapse it into a `[Pasted text #N +L lines]` reference with a stored
/// content row.
pub fn plan_text_paste(input: &TextPasteInput) -> TextPastePlan {
    let mut text = normalize_pasted_text(&input.raw_text);
    let pasted_num_lines = pasted_text_ref_num_lines(&text);
    let mut next_mode = None;

    if input.current_input_is_empty && pasted_num_lines == 0 {
        let pasted_mode = get_mode_from_input(&text);
        if pasted_mode != HistoryMode::Prompt {
            next_mode = Some(pasted_mode);
            text = get_value_from_input(&text);
        }
    }

    let num_lines = pasted_text_ref_num_lines(&text) as i32;
    if text.len() > 1_000 || num_lines >= 4 {
        let content = PromptPasteContent {
            id: input.next_paste_id,
            kind: String::from("text"),
            content: text.clone(),
            media_type: None,
            filename: None,
            source_path: None,
        };
        return TextPastePlan {
            clear_pending_space_after_pill: true,
            next_mode,
            text_to_insert: format_pasted_text_ref(input.next_paste_id, num_lines.max(0) as usize),
            new_content: Some(content),
        };
    }

    TextPastePlan {
        clear_pending_space_after_pill: true,
        next_mode,
        text_to_insert: text,
        new_content: None,
    }
}

/// Plan an image paste: build the stored image row plus the reference text
/// to insert.
pub fn plan_image_paste(input: &ImagePasteInput) -> ImagePastePlan {
    let content = PromptPasteContent {
        id: input.next_paste_id,
        kind: String::from("image"),
        content: input.image.clone(),
        media_type: Some(
            input
                .media_type
                .clone()
                .unwrap_or_else(|| String::from("image/png")),
        ),
        filename: Some(
            input
                .filename
                .clone()
                .unwrap_or_else(|| String::from("Pasted image")),
        ),
        source_path: input.source_path.clone(),
    };
    let prefix = if input.pending_space_after_pill {
        " "
    } else {
        ""
    };
    ImagePastePlan {
        text_to_insert: format!("{prefix}[Image #{}]", input.next_paste_id),
        new_content: content,
        arm_pending_space_after_pill: true,
    }
}

/// Normalize pasted text the way every paste entry point does before
/// storing or splicing it: strip ANSI escapes, canonicalize line
/// endings to `\n`, and expand tabs.
///
/// `\r\n` must collapse to a single `\n` — clipboard text read
/// directly on Windows (arboard) carries CRLF pairs, and the previous
/// bare `replace('\r', "\n")` turned each pair into a doubled blank
/// line. Terminal-delivered pastes only carry bare `\r`, which the
/// second replace still handles. Exported (`pub`) because the TUI's
/// paste echo suppressor must compare terminal-delivered chunks
/// against clipboard text in this same normalized space.
pub fn normalize_pasted_text(raw_text: &str) -> String {
    let stripped = strip_ansi_escape_sequences(raw_text);
    stripped
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

fn format_pasted_text_ref(id: u32, num_lines: usize) -> String {
    if num_lines == 0 {
        format!("[Pasted text #{id}]")
    } else {
        format!("[Pasted text #{id} +{num_lines} lines]")
    }
}

fn strip_ansi_escape_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == 0x1b {
            idx += 1;
            if idx < bytes.len() && bytes[idx] == b'[' {
                idx += 1;
                while idx < bytes.len() {
                    let byte = bytes[idx];
                    idx += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
                continue;
            }
            continue;
        }

        let ch = input[idx..].chars().next().unwrap_or_default();
        output.push(ch);
        idx += ch.len_utf8();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_paste_normalizes_ansi_cr_and_tabs() {
        let plan = plan_text_paste(&TextPasteInput {
            raw_text: "\u{1b}[31mhi\tthere\rfriend".into(),
            current_input_is_empty: false,
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(plan.text_to_insert, "hi    there\nfriend");
        assert!(plan.new_content.is_none());
        assert!(plan.clear_pending_space_after_pill);
    }

    #[test]
    fn text_paste_into_empty_prompt_can_switch_mode_and_strip_bang() {
        let plan = plan_text_paste(&TextPasteInput {
            raw_text: "!echo hi".into(),
            current_input_is_empty: true,
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(plan.next_mode, Some(HistoryMode::Bash));
        assert_eq!(plan.text_to_insert, "echo hi");
    }

    #[test]
    fn multiline_text_paste_into_empty_prompt_keeps_leading_bang_literal() {
        let plan = plan_text_paste(&TextPasteInput {
            raw_text: "!echo hi\nnext".into(),
            current_input_is_empty: true,
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(plan.next_mode, None);
        assert_eq!(plan.text_to_insert, "!echo hi\nnext");
        assert!(plan.new_content.is_none());
    }

    #[test]
    fn multiline_text_paste_into_empty_prompt_keeps_leading_question_literal() {
        let plan = plan_text_paste(&TextPasteInput {
            raw_text: "?what\nnext".into(),
            current_input_is_empty: true,
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(plan.next_mode, None);
        assert_eq!(plan.text_to_insert, "?what\nnext");
        assert!(plan.new_content.is_none());
    }

    #[test]
    fn long_or_tall_text_paste_collapses_to_reference() {
        let long = format!("{}\n{}", "a".repeat(1001), "b");
        let plan = plan_text_paste(&TextPasteInput {
            raw_text: long.clone(),
            current_input_is_empty: false,
            next_paste_id: 7,
            terminal_rows: 30,
        });
        assert_eq!(plan.text_to_insert, "[Pasted text #7 +1 lines]");
        assert_eq!(
            plan.new_content,
            Some(PromptPasteContent {
                id: 7,
                kind: "text".into(),
                content: long.replace('\r', "\n").replace('\t', "    "),
                media_type: None,
                filename: None,
                source_path: None,
            })
        );
    }

    #[test]
    fn short_two_line_text_stays_inline() {
        let plan = plan_text_paste(&TextPasteInput {
            raw_text: "one\ntwo".into(),
            current_input_is_empty: false,
            next_paste_id: 3,
            terminal_rows: 5,
        });
        assert_eq!(plan.text_to_insert, "one\ntwo");
        assert!(plan.new_content.is_none());
    }

    #[test]
    fn four_line_text_collapses_to_reference_chip() {
        let plan = plan_text_paste(&TextPasteInput {
            raw_text: "one\ntwo\nthree\nfour\nfive".into(),
            current_input_is_empty: false,
            next_paste_id: 3,
            terminal_rows: 30,
        });
        assert_eq!(plan.text_to_insert, "[Pasted text #3 +4 lines]");
        assert!(plan.new_content.is_some());
    }

    #[test]
    fn image_paste_builds_content_defaults_and_prefix_spacing() {
        let plan = plan_image_paste(&ImagePasteInput {
            next_paste_id: 9,
            image: "BASE64".into(),
            media_type: None,
            filename: None,
            source_path: Some("clip.png".into()),
            pending_space_after_pill: true,
        });
        assert_eq!(plan.text_to_insert, " [Image #9]");
        assert!(plan.arm_pending_space_after_pill);
        assert_eq!(plan.new_content.media_type.as_deref(), Some("image/png"));
        assert_eq!(plan.new_content.filename.as_deref(), Some("Pasted image"));
    }

    // ----- try_merge_into_prev_text_paste ----------------------------

    fn text_chip(id: u32, content: &str) -> PromptPasteContent {
        PromptPasteContent {
            id,
            kind: "text".into(),
            content: content.into(),
            media_type: None,
            filename: None,
            source_path: None,
        }
    }

    #[test]
    fn try_merge_appends_into_prev_text_chip_when_prompt_ends_with_ref() {
        let chip = text_chip(
            1,
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12",
        );
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("prefix {existing_ref}");
        let cursor = prompt.len();
        let contents = vec![chip.clone()];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "extra",
            current_input: &prompt,
            cursor_offset: cursor,
            pasted_contents: &contents,
        })
        .expect("merge must succeed when prompt ends with the chip ref");

        assert_eq!(result.updated_content_id, 1);
        assert_eq!(
            result.updated_content_text,
            format!("{}extra", chip.content)
        );
        let expected_ref =
            format_pasted_text_ref(1, pasted_text_ref_num_lines(&result.updated_content_text));
        assert!(result.input.ends_with(&expected_ref));
        assert_eq!(result.cursor_offset, result.input.len());
    }

    #[test]
    fn try_merge_bumps_line_count_when_new_text_has_newlines() {
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = existing_ref.clone();
        let cursor = prompt.len();
        let contents = vec![chip.clone()];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "c\nd",
            current_input: &prompt,
            cursor_offset: cursor,
            pasted_contents: &contents,
        })
        .expect("merge must succeed");

        assert_eq!(result.updated_content_text, "a\nbc\nd");
        assert_eq!(result.input, "[Pasted text #1 +2 lines]");
    }

    #[test]
    fn try_merge_normalizes_cr_and_tabs_before_appending() {
        let chip = text_chip(1, "head\n");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = existing_ref.clone();
        let contents = vec![chip.clone()];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "mid\ttail\rwrap",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect("merge must succeed");

        assert_eq!(result.updated_content_text, "head\nmid    tail\nwrap");
    }

    #[test]
    fn try_merge_returns_none_when_prompt_does_not_end_with_ref() {
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        // User typed " hi" after the chip ref — merging would step on their input.
        let prompt = format!("{existing_ref} hi");
        let contents = vec![chip];

        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_returns_none_when_cursor_not_at_end() {
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = existing_ref;
        let contents = vec![chip];

        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: &prompt,
            cursor_offset: 0,
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_returns_none_when_no_text_chip_exists() {
        let prompt = "plain text";
        let contents: Vec<PromptPasteContent> = vec![];

        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_returns_none_when_only_image_chip_exists() {
        let image = PromptPasteContent {
            id: 4,
            kind: "image".into(),
            content: "BASE64".into(),
            media_type: Some("image/png".into()),
            filename: Some("clip.png".into()),
            source_path: None,
        };
        let prompt = "[Image #4]";
        let contents = vec![image];

        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_diag_reports_no_text_chip() {
        let prompt = "plain text";
        let contents: Vec<PromptPasteContent> = vec![];
        let err = try_merge_into_prev_text_paste_diag(TryMergePasteInput {
            raw_text: "more",
            current_input: prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect_err("no chip");
        assert_eq!(err, MergeRejectReason::NoTextChip);
    }

    #[test]
    fn try_merge_diag_reports_prompt_tail_mismatch_for_ascii_tail() {
        let chip = text_chip(7, "line1\nline2");
        let existing_ref = format_pasted_text_ref(7, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref}xyz");
        let contents = vec![chip];

        let err = try_merge_into_prev_text_paste_diag(TryMergePasteInput {
            raw_text: "tail",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect_err("ascii tail should reject merge");

        assert_eq!(err.diagnostic_name(), "prompt_tail_mismatch");
        match err {
            MergeRejectReason::PromptTailMismatch {
                expected_ref,
                actual_tail,
            } => {
                assert_eq!(expected_ref, existing_ref);
                assert!(actual_tail.ends_with("xyz"), "actual_tail={actual_tail:?}");
            }
            other => panic!("expected PromptTailMismatch, got {other:?}"),
        }
    }

    #[test]
    fn try_merge_diag_reports_cursor_not_at_end() {
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = existing_ref;
        let contents = vec![chip];
        let err = try_merge_into_prev_text_paste_diag(TryMergePasteInput {
            raw_text: "more",
            current_input: &prompt,
            cursor_offset: 0,
            pasted_contents: &contents,
        })
        .expect_err("should reject");
        assert!(matches!(
            err,
            MergeRejectReason::CursorNotAtEnd {
                cursor_offset: 0,
                ..
            }
        ));
    }

    #[test]
    fn try_merge_absorbs_non_ascii_stray_tail_between_chunks() {
        // Regression: conpty splits bracketed paste at non-ASCII byte
        // boundaries and delivers the straddling char as a raw Key
        // event that lands between the chip ref and the next paste.
        // With only the strict `ends_with` guard, the next chunk
        // refused to merge and the user saw N chips for one paste.
        let chip = text_chip(1, "line1\nline2\nline3");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref}—");
        let contents = vec![chip];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "line4",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect("stray em-dash tail must be absorbed into merge");

        assert_eq!(result.updated_content_id, 1);
        assert_eq!(
            result.updated_content_text, "line1\nline2\nline3—line4",
            "stray tail folded in before the new paste body"
        );
        assert_eq!(result.input, format_pasted_text_ref(1, 2));
        assert_eq!(
            result.absorbed_stray_tail_bytes, 3,
            "em-dash tail reported via absorbed_stray_tail_bytes (U+2014 = 3 bytes UTF-8)"
        );
    }

    #[test]
    fn try_merge_exact_match_reports_zero_stray_tail() {
        // Primary (no fallback) path must report zero stray bytes so
        // the runner can differentiate "clean merge" from "heuristic
        // fallback fired" in tracing without recomputing.
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let contents = vec![chip];
        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "c",
            current_input: &existing_ref,
            cursor_offset: existing_ref.len(),
            pasted_contents: &contents,
        })
        .expect("exact match merges");
        assert_eq!(result.absorbed_stray_tail_bytes, 0);
    }

    #[test]
    fn try_merge_rejects_cjk_tail_like_ascii_tail_when_cursor_moved() {
        // Safety net for the known soft spot of the non-ASCII
        // heuristic: if a user types CJK between pastes AND moves
        // the cursor elsewhere, the cursor-at-end guard still blocks
        // merge. This asserts that cursor-movement protection layers
        // on top of non-ASCII tolerance, so a user editing after
        // typing "好" isn't silently fused into the chip.
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref}好");
        let contents = vec![chip];
        // Cursor BEFORE the "好" — user moved back to edit.
        let cursor = existing_ref.len();
        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: &prompt,
            cursor_offset: cursor,
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_absorbs_multiple_non_ascii_stray_chars() {
        let chip = text_chip(1, "head");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref}—…");
        let contents = vec![chip];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "tail",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect("multi-codepoint non-ASCII tail must still merge");

        assert_eq!(result.updated_content_text, "head—…tail");
    }

    #[test]
    fn try_merge_absorbs_ui_suffix_pollution_after_chip_ref() {
        let chip = text_chip(1, "line1\nline2\nline3");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref}· ∴");
        let contents = vec![chip];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "line4",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect("short UI-ish tail should merge");

        assert_eq!(result.updated_content_text, "line1\nline2\nline3· ∴line4");
        assert_eq!(result.input, format_pasted_text_ref(1, 2));
        assert_eq!(result.absorbed_stray_tail_bytes, "· ∴".len());
    }

    #[test]
    fn try_merge_absorbs_thinking_suffix_pollution_after_chip_ref() {
        let chip = text_chip(1, "alpha\nbeta");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref}Thinking…");
        let contents = vec![chip];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "\ngamma",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect("Thinking suffix should merge");

        assert_eq!(result.updated_content_text, "alpha\nbetaThinking…\ngamma");
        assert_eq!(result.input, format_pasted_text_ref(1, 2));
    }

    #[test]
    fn try_merge_still_rejects_short_ascii_tail() {
        // User could have typed "hi" after the chip — ASCII tails
        // are NEVER absorbed, even though they sit inside the
        // MAX_STRAY_TAIL_BYTES window.
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref}hi");
        let contents = vec![chip];

        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_rejects_user_space_between_pastes() {
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        let prompt = format!("{existing_ref} ");
        let contents = vec![chip];

        assert!(
            try_merge_into_prev_text_paste(TryMergePasteInput {
                raw_text: "c\nd",
                current_input: &prompt,
                cursor_offset: prompt.len(),
                pasted_contents: &contents,
            })
            .is_none(),
            "a separator typed between two paste actions must keep the next paste separate"
        );
    }

    #[test]
    fn try_merge_rejects_stray_tail_longer_than_limit() {
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        // Long non-ASCII tail — beyond MAX_STRAY_TAIL_BYTES (48).
        let long_tail: String = std::iter::repeat('—').take(17).collect(); // 51 bytes
        let prompt = format!("{existing_ref}{long_tail}");
        let contents = vec![chip];

        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_rejects_stray_tail_with_newline() {
        let chip = text_chip(1, "a\nb");
        let existing_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip.content));
        // Newline in tail signals user moved on — don't absorb.
        let prompt = format!("{existing_ref}—\n");
        let contents = vec![chip];

        assert!(try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "more",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .is_none());
    }

    #[test]
    fn try_merge_four_chip_split_paste_coalesces_into_one() {
        // End-to-end scenario from the user's log: bracketed paste
        // split into 4 Event::Paste chunks with em-dash stragglers
        // between each chunk. With the fallback absorbing each stray
        // em-dash into the ongoing chip, all four chunks collapse
        // into chip #1.
        let chunk_a: String = (0..20).map(|i| format!("lineA{i}\n")).collect();
        let mut state = apply_text_paste(ApplyTextPasteState {
            raw_text: chunk_a.clone(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(state.pasted_contents.len(), 1, "first chunk made chip #1");
        // Simulate the em-dash Key event landing between chunks.
        state.input.push('—');
        let remaining_chunks = [
            (0..5).map(|i| format!("lineB{i}\n")).collect::<String>(),
            (0..4).map(|i| format!("lineC{i}\n")).collect::<String>(),
            (0..30).map(|i| format!("lineD{i}\n")).collect::<String>(),
        ];
        for (idx, chunk) in remaining_chunks.iter().enumerate() {
            let merged = try_merge_into_prev_text_paste(TryMergePasteInput {
                raw_text: chunk,
                current_input: &state.input,
                cursor_offset: state.input.len(),
                pasted_contents: &state.pasted_contents,
            })
            .expect("each chunk must fold into chip #1");
            state.input = merged.input;
            state.cursor_offset = merged.cursor_offset;
            if let Some(chip) = state
                .pasted_contents
                .iter_mut()
                .find(|c| c.id == merged.updated_content_id)
            {
                chip.content = merged.updated_content_text;
            }
            // Next em-dash straggler between chunks (not after the last).
            if idx + 1 < remaining_chunks.len() {
                state.input.push('—');
            }
        }
        assert_eq!(state.pasted_contents.len(), 1, "only one chip survived");
        assert_eq!(state.pasted_contents[0].id, 1);
        assert!(state.pasted_contents[0].content.contains('—'));
    }

    #[test]
    fn try_merge_diag_succeeds_and_matches_option_variant() {
        let chip = text_chip(7, "line1\nline2");
        let existing_ref = format_pasted_text_ref(7, pasted_text_ref_num_lines(&chip.content));
        let contents = vec![chip];
        let ok = try_merge_into_prev_text_paste_diag(TryMergePasteInput {
            raw_text: "\nline3",
            current_input: &existing_ref,
            cursor_offset: existing_ref.len(),
            pasted_contents: &contents,
        })
        .expect("merge must succeed");
        assert_eq!(ok.updated_content_id, 7);
        assert_eq!(ok.updated_content_text, "line1\nline2\nline3");
    }

    #[test]
    fn try_merge_targets_most_recent_text_chip_not_earlier_ones() {
        // Even if an older chip exists earlier in the prompt, the merge
        // must target the LAST-added text chip (that's the one the
        // runner just flushed moments ago).
        let chip_old = text_chip(1, "old\ncontent");
        let chip_new = text_chip(2, "new\ncontent");
        let old_ref = format_pasted_text_ref(1, pasted_text_ref_num_lines(&chip_old.content));
        let new_ref = format_pasted_text_ref(2, pasted_text_ref_num_lines(&chip_new.content));
        let prompt = format!("{old_ref} middle {new_ref}");
        let contents = vec![chip_old, chip_new];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "extra",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect("merge must succeed");

        assert_eq!(result.updated_content_id, 2);
        assert_eq!(result.updated_content_text, "new\ncontentextra");
        // Old chip ref is preserved at the start of the prompt.
        assert!(result.input.starts_with(&old_ref));
    }

    #[test]
    fn split_paste_flush_then_merge_produces_single_chip_with_combined_content() {
        // Integration scenario mirroring what `flush_burst_with_merge`
        // does on `AppState`: the burst detector split one logical
        // paste into two flushes (conpty batch gap > BURST_IDLE_TIMEOUT).
        // First flush goes through `apply_text_paste`; second flush
        // arrives within the TUI merge grace window and routes through
        // `try_merge_into_prev_text_paste`. Result must be a SINGLE
        // chip whose content contains both halves.
        let first_flush =
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12";
        let second_flush = "\ntail";

        // First flush lands as a fresh chip.
        let first_result = apply_text_paste(ApplyTextPasteState {
            raw_text: first_flush.into(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(first_result.input, "[Pasted text #1 +11 lines]");
        assert_eq!(first_result.pasted_contents.len(), 1);
        assert_eq!(first_result.next_paste_id, 2);

        // Second flush: runner sees "within grace window" and calls
        // `try_merge_into_prev_text_paste` instead of `apply_text_paste`.
        let merged = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: second_flush,
            current_input: &first_result.input,
            cursor_offset: first_result.cursor_offset,
            pasted_contents: &first_result.pasted_contents,
        })
        .expect("within-grace flush must merge");

        // Exactly ONE chip, line count bumped by the newly-appended newline.
        assert_eq!(merged.updated_content_id, 1);
        assert_eq!(merged.input, "[Pasted text #1 +12 lines]");
        assert_eq!(
            merged.updated_content_text,
            format!("{first_flush}{second_flush}")
        );
        assert_eq!(merged.cursor_offset, merged.input.len());
        // next_paste_id is NOT advanced — no new chip was created.
    }

    #[test]
    fn burst_idle_flush_with_non_ascii_tail_merges_into_existing_chip() {
        // Integration scenario for the specific Windows-conpty bug that
        // motivated the fallback:
        //
        //   1. First bracketed paste chunk creates chip #1.
        //   2. conpty emits a single non-ASCII char (em-dash here) as
        //      a raw Event::Key(Char) — the runner's PasteBurst folds
        //      it into its buffer and eventually flushes it to the
        //      prompt because no further burst chars arrive.
        //   3. By the time the SECOND bracketed chunk arrives, the
        //      prompt ends with `{chip_ref}{em_dash}`, NOT just
        //      `{chip_ref}`. Without the stray-tail fallback the merge
        //      guard rejects and a second chip is created.
        //
        // With the fallback the stray em-dash is absorbed INTO chip #1
        // BEFORE the second chunk is appended, so only one chip exists.
        let first_flush: String = (0..12).map(|i| format!("line{i}\n")).collect();
        let first_result = apply_text_paste(ApplyTextPasteState {
            raw_text: first_flush.clone(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(first_result.pasted_contents.len(), 1);

        // Simulate burst-idle-flush of a lone em-dash: PasteBurst
        // flushes '—' into app.input which now ends with chip_ref + '—'.
        let mut prompt_with_stray = first_result.input.clone();
        prompt_with_stray.push('—');

        let second_chunk = "tail\npart";
        let merged = try_merge_into_prev_text_paste_diag(TryMergePasteInput {
            raw_text: second_chunk,
            current_input: &prompt_with_stray,
            cursor_offset: prompt_with_stray.len(),
            pasted_contents: &first_result.pasted_contents,
        })
        .expect("burst-idle stray em-dash must not block merge");

        assert_eq!(
            merged.updated_content_id, 1,
            "second chunk must fold into chip #1, not spawn chip #2"
        );
        assert_eq!(
            merged.absorbed_stray_tail_bytes, 3,
            "absorbed_stray_tail_bytes must report the em-dash's 3 UTF-8 bytes \
             so flush_burst_with_merge can emit the diagnostic warn log"
        );
        assert!(
            merged.updated_content_text.contains('—'),
            "stray em-dash must be preserved in the merged chip content"
        );
        assert!(
            merged.updated_content_text.ends_with(second_chunk),
            "second chunk must land AFTER the stray tail in chip content"
        );
    }

    #[test]
    fn split_paste_outside_merge_grace_falls_back_to_two_chips() {
        // Same split scenario but simulating "grace window elapsed"
        // by calling `apply_text_paste` a second time (what the runner
        // does when `last_paste_flush_at` is older than the merge
        // grace). Confirms that the two-chip outcome is exactly
        // what `try_merge_into_prev_text_paste` exists to prevent.
        let first_result = apply_text_paste(ApplyTextPasteState {
            raw_text: "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12".into(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        let second_result = apply_text_paste(ApplyTextPasteState {
            raw_text: "\ntail\nmore\nextra\nfinal".into(),
            current_input: first_result.input.clone(),
            cursor_offset: first_result.cursor_offset,
            pasted_contents: first_result.pasted_contents.clone(),
            next_paste_id: first_result.next_paste_id,
            terminal_rows: 30,
        });

        assert_eq!(
            second_result.input, "[Pasted text #1 +11 lines][Pasted text #2 +4 lines]",
            "without merge, split paste renders as two adjacent chips"
        );
        assert_eq!(second_result.pasted_contents.len(), 2);
    }

    #[test]
    fn try_merge_preserves_prefix_text_before_chip_reference() {
        let chip = text_chip(5, "x\ny");
        let existing_ref = format_pasted_text_ref(5, pasted_text_ref_num_lines(&chip.content));
        let prefix = "look at this: ";
        let prompt = format!("{prefix}{existing_ref}");
        let contents = vec![chip];

        let result = try_merge_into_prev_text_paste(TryMergePasteInput {
            raw_text: "\nz",
            current_input: &prompt,
            cursor_offset: prompt.len(),
            pasted_contents: &contents,
        })
        .expect("merge must succeed");

        assert_eq!(result.updated_content_text, "x\ny\nz");
        assert_eq!(result.input, format!("{prefix}[Pasted text #5 +2 lines]"));
    }

    // ----- PasteFragment / PasteBurstBuilder -------------------------

    #[test]
    fn paste_fragment_char_pushes_single_char() {
        let mut buf = String::new();
        PasteFragment::Char('a').push_into(&mut buf);
        PasteFragment::Char('字').push_into(&mut buf);
        assert_eq!(buf, "a字");
    }

    #[test]
    fn paste_fragment_enter_pushes_newline() {
        let mut buf = String::new();
        PasteFragment::Enter.push_into(&mut buf);
        assert_eq!(buf, "\n");
    }

    #[test]
    fn paste_fragment_tab_pushes_raw_tab() {
        let mut buf = String::new();
        PasteFragment::Tab.push_into(&mut buf);
        assert_eq!(buf, "\t");
    }

    #[test]
    fn paste_burst_builder_starts_empty() {
        let b = PasteBurstBuilder::new();
        assert_eq!(b.count(), 0);
        assert!(!b.contains_newline());
        assert_eq!(b.as_text(), "");
    }

    #[test]
    fn paste_burst_builder_accumulates_text() {
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Char('h'));
        b.push(PasteFragment::Char('i'));
        assert_eq!(b.count(), 2);
        assert!(!b.contains_newline());
        assert_eq!(b.as_text(), "hi");
        assert_eq!(b.into_text(), "hi");
    }

    #[test]
    fn paste_burst_builder_tracks_enter_as_newline() {
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Char('a'));
        b.push(PasteFragment::Enter);
        b.push(PasteFragment::Char('b'));
        assert_eq!(b.count(), 3);
        assert!(b.contains_newline());
        assert_eq!(b.as_text(), "a\nb");
    }

    #[test]
    fn paste_burst_builder_handles_tab_and_mixed_fragments() {
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Char('-'));
        b.push(PasteFragment::Char(' '));
        b.push(PasteFragment::Char('['));
        b.push(PasteFragment::Char('x'));
        b.push(PasteFragment::Char(']'));
        b.push(PasteFragment::Tab);
        b.push(PasteFragment::Char('t'));
        b.push(PasteFragment::Enter);
        assert_eq!(b.count(), 8);
        assert!(b.contains_newline());
        assert_eq!(b.as_text(), "- [x]\tt\n");
    }

    #[test]
    fn should_apply_paste_burst_empty_is_false() {
        let b = PasteBurstBuilder::new();
        assert!(!should_apply_paste_burst(&b));
    }

    #[test]
    fn should_apply_paste_burst_single_char_is_false() {
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Char('a'));
        assert!(!should_apply_paste_burst(&b));
    }

    #[test]
    fn should_apply_paste_burst_single_enter_is_false_startup_regression() {
        // Regression for "enabling it left a spurious \n and a second line":
        // on Windows, the shell's Enter that launched rebon can
        // leak its Press event into rebon's queue. With a 1-Enter
        // burst the old code returned true (because "contains
        // newline"), which applied a `"\n"` paste to an empty
        // input. A single Enter must NEVER qualify as a paste —
        // it's just a keystroke, and the normal key path will
        // turn it into Submit (a no-op on empty input) instead of
        // silently inserting a blank line.
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Enter);
        assert!(!should_apply_paste_burst(&b));
    }

    #[test]
    fn should_apply_paste_burst_two_enters_is_true() {
        // Two Enter fragments (genuine multi-newline paste) still
        // must be applied as a paste so the newlines don't
        // trigger Submit mid-paste.
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Enter);
        b.push(PasteFragment::Enter);
        assert!(should_apply_paste_burst(&b));
    }

    #[test]
    fn should_apply_paste_burst_char_plus_enter_is_true() {
        // Minimum viable "real paste": at least one char followed
        // by a newline. This is the smallest burst that clearly
        // isn't a single keystroke.
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Char('a'));
        b.push(PasteFragment::Enter);
        assert!(should_apply_paste_burst(&b));
    }

    #[test]
    fn should_apply_paste_burst_two_chars_is_true() {
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Char('a'));
        b.push(PasteFragment::Char('b'));
        assert!(should_apply_paste_burst(&b));
    }

    #[test]
    fn should_apply_paste_burst_large_non_newline_burst_is_true() {
        let mut b = PasteBurstBuilder::new();
        for c in "hello world".chars() {
            b.push(PasteFragment::Char(c));
        }
        assert!(!b.contains_newline());
        assert!(should_apply_paste_burst(&b));
    }

    #[test]
    fn should_apply_paste_burst_task_list_with_many_lines_is_true() {
        // The exact scenario from the bug report: a markdown task
        // list pasted into the prompt input with many embedded
        // newlines. Each Enter would otherwise trigger submit.
        let mut b = PasteBurstBuilder::new();
        let task_list = "- [x] first\n- [x] second\n- [x] third\n";
        for ch in task_list.chars() {
            if ch == '\n' {
                b.push(PasteFragment::Enter);
            } else {
                b.push(PasteFragment::Char(ch));
            }
        }
        assert!(b.contains_newline());
        assert!(should_apply_paste_burst(&b));
        assert_eq!(b.as_text(), task_list);
    }

    #[test]
    fn paste_burst_feeds_into_apply_text_paste_as_single_splice() {
        // End-to-end: collect a multi-line burst in a builder,
        // then feed the aggregated text through apply_text_paste
        // and confirm it collapses to a reference chip rather than
        // inserting raw newlines that would trigger submit.
        let mut b = PasteBurstBuilder::new();
        let task_list = "- [x] one\n- [x] two\n- [x] three\n- [x] four\n";
        for ch in task_list.chars() {
            if ch == '\n' {
                b.push(PasteFragment::Enter);
            } else {
                b.push(PasteFragment::Char(ch));
            }
        }
        assert!(should_apply_paste_burst(&b));

        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: b.into_text(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "[Pasted text #1 +4 lines]");
        assert_eq!(result.pasted_contents.len(), 1);
        assert!(result.pasted_contents[0].content.contains("- [x] one"));
        assert!(result.pasted_contents[0].content.contains("- [x] four"));
        assert_eq!(result.next_paste_id, 2);
    }

    #[test]
    fn paste_burst_short_burst_no_newline_inserts_inline_text() {
        let mut b = PasteBurstBuilder::new();
        for ch in "world".chars() {
            b.push(PasteFragment::Char(ch));
        }
        assert!(should_apply_paste_burst(&b));
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: b.into_text(),
            current_input: "hello ".into(),
            cursor_offset: 6,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "hello world");
        assert_eq!(result.cursor_offset, 11);
        assert!(result.pasted_contents.is_empty());
    }

    #[test]
    fn paste_burst_with_five_total_lines_collapses_to_reference() {
        // Up to 4 lines (3 newlines) stay inline; 5+ lines collapse.
        let mut b = PasteBurstBuilder::new();
        for ch in "a\nb\nc\nd\ne".chars() {
            if ch == '\n' {
                b.push(PasteFragment::Enter);
            } else {
                b.push(PasteFragment::Char(ch));
            }
        }
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: b.into_text(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert!(result.input.starts_with("[Pasted text #1 "));
        assert!(result.input.contains("+4 lines"));
    }

    #[test]
    fn paste_burst_with_three_total_lines_stays_inline() {
        let mut b = PasteBurstBuilder::new();
        for ch in "a\nb\nc".chars() {
            if ch == '\n' {
                b.push(PasteFragment::Enter);
            } else {
                b.push(PasteFragment::Char(ch));
            }
        }
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: b.into_text(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "a\nb\nc");
        assert!(result.pasted_contents.is_empty());
    }

    #[test]
    fn paste_burst_tab_expands_to_spaces_via_plan_normalizer() {
        // Tab becomes literal \t in the burst; plan_text_paste then
        // normalizes \t → four spaces. This test guards that chain.
        let mut b = PasteBurstBuilder::new();
        b.push(PasteFragment::Char('a'));
        b.push(PasteFragment::Tab);
        b.push(PasteFragment::Char('b'));
        assert_eq!(b.as_text(), "a\tb");
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: b.into_text(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "a    b");
    }

    #[test]
    fn paste_burst_bang_prefix_in_empty_prompt_switches_mode() {
        // A burst beginning with `!` in an empty prompt should
        // flip the prompt into Bash history mode and strip the
        // prefix — matches the single-Event::Paste behavior.
        let mut b = PasteBurstBuilder::new();
        for ch in "!ls -la".chars() {
            b.push(PasteFragment::Char(ch));
        }
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: b.into_text(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "ls -la");
        assert_eq!(result.next_mode, Some(HistoryMode::Bash));
    }

    // ----- apply_text_paste ------------------------------------------

    #[test]
    fn apply_text_paste_inserts_small_text_at_cursor() {
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: "world".into(),
            current_input: "hello  !".into(),
            cursor_offset: 6,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "hello world !");
        assert_eq!(result.cursor_offset, 11);
        assert!(result.pasted_contents.is_empty());
        assert_eq!(result.next_paste_id, 1);
        assert_eq!(result.next_mode, None);
    }

    #[test]
    fn apply_text_paste_collapses_multiline_to_reference_chip_and_stores_content() {
        let raw = "line1\nline2\nline3\nline4\nline5";
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: raw.into(),
            current_input: "prefix ".into(),
            cursor_offset: 7,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "prefix [Pasted text #1 +4 lines]");
        assert_eq!(
            result.cursor_offset,
            "prefix [Pasted text #1 +4 lines]".len()
        );
        assert_eq!(result.pasted_contents.len(), 1);
        assert_eq!(result.pasted_contents[0].id, 1);
        assert_eq!(result.pasted_contents[0].content, raw);
        assert_eq!(result.next_paste_id, 2);
    }

    #[test]
    fn apply_text_paste_splice_preserves_text_after_cursor() {
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: "XYZ".into(),
            current_input: "abc def".into(),
            cursor_offset: 3,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "abcXYZ def");
        assert_eq!(result.cursor_offset, 6);
    }

    #[test]
    fn apply_text_paste_bang_mode_on_empty_prompt_strips_bang_and_returns_mode() {
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: "!echo hi".into(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "echo hi");
        assert_eq!(result.next_mode, Some(HistoryMode::Bash));
    }

    #[test]
    fn apply_text_paste_appends_new_content_without_dropping_existing_store() {
        let existing = PromptPasteContent {
            id: 9,
            kind: "text".into(),
            content: "earlier".into(),
            media_type: None,
            filename: None,
            source_path: None,
        };
        let raw = "a\nb\nc\nd\ne";
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: raw.into(),
            current_input: String::new(),
            cursor_offset: 0,
            pasted_contents: vec![existing.clone()],
            next_paste_id: 10,
            terminal_rows: 30,
        });
        assert_eq!(result.pasted_contents.len(), 2);
        assert_eq!(result.pasted_contents[0], existing);
        assert_eq!(result.pasted_contents[1].id, 10);
        assert_eq!(result.next_paste_id, 11);
    }

    #[test]
    fn apply_text_paste_clamps_cursor_offset_past_end_of_input() {
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: "!".into(),
            current_input: "hi".into(),
            cursor_offset: 999,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "hi!");
        assert_eq!(result.cursor_offset, 3);
    }

    #[test]
    fn apply_text_paste_snaps_cursor_to_utf8_boundary() {
        let result = apply_text_paste(ApplyTextPasteState {
            raw_text: "!".into(),
            current_input: "你a".into(),
            cursor_offset: 2,
            pasted_contents: Vec::new(),
            next_paste_id: 1,
            terminal_rows: 30,
        });
        assert_eq!(result.input, "!你a");
        assert_eq!(result.cursor_offset, 1);
    }

    // ── normalize_pasted_text ─────────────────────────────────────

    #[test]
    fn normalize_pasted_text_collapses_crlf_to_single_newline() {
        // Clipboard text read directly on Windows carries CRLF pairs.
        // Each pair must become exactly one `\n`, not the doubled
        // blank line the earlier bare `replace('\r', "\n")` produced.
        assert_eq!(normalize_pasted_text("a\r\nb\r\nc"), "a\nb\nc");
        // Terminal-delivered pastes carry bare `\r` — still one `\n`.
        assert_eq!(normalize_pasted_text("a\rb\rc"), "a\nb\nc");
        // Mixed content stays consistent.
        assert_eq!(normalize_pasted_text("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn normalize_pasted_text_is_idempotent() {
        let raw = "x\r\ny\tz\x1b[31mred\x1b[0m";
        let once = normalize_pasted_text(raw);
        assert_eq!(normalize_pasted_text(&once), once);
    }

    // ── repair_overadopted_text_chip ──────────────────────────────

    #[test]
    fn repair_overadopted_chip_trims_unconfirmed_suffix_and_relabels() {
        let adopted = "l1\nl2\nl3\nl4\nl5";
        let chips = vec![text_chip(1, adopted)];
        let old_ref = "[Pasted text #1 +4 lines]";
        let result = repair_overadopted_text_chip(OveradoptedChipRepairInput {
            current_input: old_ref,
            cursor_offset: old_ref.len(),
            pasted_contents: &chips,
            chip_id: 1,
            adopted_norm: adopted,
            confirmed_len: "l1\nl2\nl3".len(),
        })
        .expect("repair must apply");
        assert_eq!(result.updated_content_text, "l1\nl2\nl3");
        assert_eq!(result.input, "[Pasted text #1 +2 lines]");
        assert_eq!(result.cursor_offset, result.input.len());
        assert_eq!(result.trimmed_bytes, "\nl4\nl5".len());
    }

    #[test]
    fn repair_overadopted_chip_preserves_prior_merged_content() {
        // The chip may hold earlier merged content before the adopted
        // suffix — only the adopted portion may be trimmed.
        let adopted = "b1\nb2\nb3\nb4";
        let chips = vec![text_chip(2, &format!("prior\n{adopted}"))];
        let old_ref = "[Pasted text #2 +4 lines]";
        let result = repair_overadopted_text_chip(OveradoptedChipRepairInput {
            current_input: old_ref,
            cursor_offset: old_ref.len(),
            pasted_contents: &chips,
            chip_id: 2,
            adopted_norm: adopted,
            confirmed_len: "b1\nb2".len(),
        })
        .expect("repair must apply");
        assert_eq!(result.updated_content_text, "prior\nb1\nb2");
        assert_eq!(result.input, "[Pasted text #2 +2 lines]");
    }

    #[test]
    fn repair_overadopted_chip_noops_when_fully_confirmed() {
        let adopted = "a\nb\nc\nd";
        let chips = vec![text_chip(1, adopted)];
        assert!(repair_overadopted_text_chip(OveradoptedChipRepairInput {
            current_input: "[Pasted text #1 +3 lines]",
            cursor_offset: 0,
            pasted_contents: &chips,
            chip_id: 1,
            adopted_norm: adopted,
            confirmed_len: adopted.len(),
        })
        .is_none());
    }

    #[test]
    fn repair_overadopted_chip_snaps_confirmed_len_to_char_boundary() {
        let adopted = "你好\n世界\n第三\n第四";
        let chips = vec![text_chip(1, adopted)];
        let old_ref = "[Pasted text #1 +3 lines]";
        // Mid-codepoint confirmed length: one byte into '好'.
        let result = repair_overadopted_text_chip(OveradoptedChipRepairInput {
            current_input: old_ref,
            cursor_offset: old_ref.len(),
            pasted_contents: &chips,
            chip_id: 1,
            adopted_norm: adopted,
            confirmed_len: "你".len() + 1,
        })
        .expect("repair must apply");
        assert_eq!(result.updated_content_text, "你");
        assert_eq!(result.input, "[Pasted text #1]");
    }

    #[test]
    fn repair_overadopted_chip_leaves_edited_prompt_tail_alone() {
        // If the prompt no longer ends with the chip ref, only the
        // stored content is trimmed; the visible input stays.
        let adopted = "a\nb\nc\nd";
        let chips = vec![text_chip(1, adopted)];
        let input = "[Pasted text #1 +3 lines] trailing";
        let result = repair_overadopted_text_chip(OveradoptedChipRepairInput {
            current_input: input,
            cursor_offset: 3,
            pasted_contents: &chips,
            chip_id: 1,
            adopted_norm: adopted,
            confirmed_len: 1,
        })
        .expect("repair must apply");
        assert_eq!(result.input, input);
        assert_eq!(result.cursor_offset, 3);
        assert_eq!(result.updated_content_text, "a");
    }

    #[test]
    fn prune_orphaned_image_ids_only_removes_unreferenced_images() {
        let ids = prune_orphaned_image_ids(
            "[Image #1] [Pasted text #2]",
            &[
                PromptPasteContent {
                    id: 1,
                    kind: "image".into(),
                    content: "img".into(),
                    media_type: None,
                    filename: None,
                    source_path: None,
                },
                PromptPasteContent {
                    id: 2,
                    kind: "text".into(),
                    content: "txt".into(),
                    media_type: None,
                    filename: None,
                    source_path: None,
                },
                PromptPasteContent {
                    id: 3,
                    kind: "image".into(),
                    content: "gone".into(),
                    media_type: None,
                    filename: None,
                    source_path: None,
                },
            ],
        );
        assert_eq!(ids, vec![3]);
    }
}
