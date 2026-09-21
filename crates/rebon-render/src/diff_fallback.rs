//! Pure-logic word-level diff renderer, used where whole-line diff output is
//! not enough and word-level highlighting is wanted.
//!
//! The pipeline runs in five steps:
//!
//! 1. [`transform_lines_to_objects`] strips the leading `+`/`-`/space from each
//!    raw patch line and classifies it.
//! 2. [`process_adjacent_lines`] pairs a remove burst with the add burst that
//!    follows it and tags the pairs as eligible for word-level diffing.
//! 3. [`number_diff_lines`] assigns 1-based line numbers using unified-diff
//!    semantics.
//! 4. [`decide_word_diff_path`] decides, per pair, whether the change is small
//!    enough to be worth word-level rendering.
//! 5. [`format_diff_lines`] renders the whole thing into backend-neutral rows.
//!
//! Five load-bearing properties:
//!
//! 1. **Line shape: `+`/`-`/space prefix.** The first character is stripped
//!    and the line classified in one step. Anything that doesn't start with `+`
//!    or `-` is [`LineType::Nochange`], and *every* branch strips that first
//!    character — so a context line loses its leading space too.
//! 2. **Adjacent grouping is one-shot, not nested.** [`process_adjacent_lines`]
//!    walks linearly: on a remove it gathers all consecutive removes, then all
//!    consecutive adds, and pairs them by index up to `min(removes, adds)`.
//!    Pairs are tagged `word_diff = true` and cross-referenced through
//!    `matched_line`; unpaired removes and adds are still pushed, untagged.
//! 3. **Numbering uses unified-diff line semantics.** [`number_diff_lines`]
//!    advances the counter on `Nochange` and `Add`. Consecutive `Remove` lines
//!    are special-cased: the first is emitted at the current number, each
//!    following one advances the counter, and the counter is then **rewound** by
//!    the number of removes pushed, so the next non-remove sees the original
//!    number (three removes starting at 10 are `10, 11, 12`, and the next line
//!    still starts at `10`). It uses a small queue and rewinds the counter after
//!    a remove burst — see the test cases for the exact behaviour.
//! 4. **Word-diff fallback threshold.** A pair is word-diffed only when its
//!    change ratio is at or below `CHANGE_THRESHOLD` (0.4) and the render is
//!    not dim. The ratio is `sum(parts that are added or removed) /
//!    (old_len + new_len)`. Above the threshold the pair falls back to
//!    whole-line rendering.
//! 5. **Manual word-diff wrapping.** The word-diff renderer walks the diff
//!    parts and wraps each one to the available content width, accumulating
//!    into a current line until either the next part wouldn't fit or the part
//!    itself is multi-line. The wrapper is injected.
//!
//! ## Why the word-differ and wrapper are injected
//!
//! Word-level diffing and ANSI-aware wrapping each have their own owner, so
//! this module takes both as `Fn`-trait parameters: the algorithm can be
//! tested with hand-crafted differs and wrappers and the production caller can
//! plug in the real implementations.

use rebon_width::WidthStr;

/// Threshold above which a paired remove/add line falls back to whole-line
/// diff rendering.
pub const CHANGE_THRESHOLD: f64 = 0.4;

/// One classified line of the patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineObject {
    /// Line content with the leading `+`/`-`/space prefix stripped.
    pub code: String,
    /// Line number (1-based, populated later by `number_diff_lines`).
    pub i: usize,
    /// Classification.
    pub line_type: LineType,
    /// Original (pre-stripped) source line — kept for word diff input.
    pub original_code: String,
    /// `true` if this line is part of a remove+add pair eligible for
    /// word-level diff. Set by [`process_adjacent_lines`].
    pub word_diff: bool,
    /// Index of the paired line in the same vector, when
    /// `word_diff == true`. An index rather than a reference, because the
    /// vector is the authoritative storage and indices are clone-friendly.
    pub matched_line: Option<usize>,
}

/// Line classification: `add`, `remove` or `nochange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineType {
    Add,
    Remove,
    Nochange,
}

/// One word-diff segment. The kind is an enum rather than a pair of optional
/// booleans, because "neither added nor removed" is the natural common case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPart {
    pub value: String,
    pub kind: DiffPartKind,
}

/// Per-segment classification for [`DiffPart`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffPartKind {
    /// Common to both sides — rendered without highlight.
    Common,
    /// Added in the new side — rendered with the added background.
    Added,
    /// Removed from the old side — rendered with the removed background.
    Removed,
}

impl DiffPart {
    pub fn common(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: DiffPartKind::Common,
        }
    }
    pub fn added(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: DiffPartKind::Added,
        }
    }
    pub fn removed(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: DiffPartKind::Removed,
        }
    }
}

pub fn calculate_word_diff(old: &str, new: &str) -> Vec<DiffPart> {
    if old == new {
        return vec![DiffPart::common(old)];
    }

    let prefix_chars = old
        .chars()
        .zip(new.chars())
        .take_while(|(old, new)| old == new)
        .count();
    let prefix_bytes: usize = old.chars().take(prefix_chars).map(char::len_utf8).sum();
    let old_after = &old[prefix_bytes..];
    let new_after = &new[prefix_bytes..];

    let suffix_chars = old_after
        .chars()
        .rev()
        .zip(new_after.chars().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let old_suffix_bytes: usize = old_after
        .chars()
        .rev()
        .take(suffix_chars)
        .map(char::len_utf8)
        .sum();
    let new_suffix_bytes: usize = new_after
        .chars()
        .rev()
        .take(suffix_chars)
        .map(char::len_utf8)
        .sum();
    let old_mid_end = old_after.len() - old_suffix_bytes;
    let new_mid_end = new_after.len() - new_suffix_bytes;

    let mut parts = Vec::new();
    if prefix_bytes > 0 {
        parts.push(DiffPart::common(&old[..prefix_bytes]));
    }
    if old_mid_end > 0 {
        parts.push(DiffPart::removed(&old_after[..old_mid_end]));
    }
    if new_mid_end > 0 {
        parts.push(DiffPart::added(&new_after[..new_mid_end]));
    }
    if old_suffix_bytes > 0 {
        parts.push(DiffPart::common(&old_after[old_mid_end..]));
    }
    parts
}

/// Decision returned by [`decide_word_diff_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordDiffDecision {
    /// Proceed with word-level rendering.
    UseWordDiff,
    /// Fall back to whole-line rendering — the change is too big or
    /// the caller asked for dim mode.
    FallbackToWholeLine,
}

/// Background color intent for a whole rendered diff line. The
/// concrete RGB values are resolved by the consumer's theme layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineColor {
    /// Added line, normal mode.
    Added,
    /// Added line, dim mode.
    AddedDimmed,
    /// Removed line, normal mode.
    Removed,
    /// Removed line, dim mode.
    RemovedDimmed,
    /// No background — `nochange` lines.
    None,
}

/// Background color intent for a single word inside a word-diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WordColor {
    AddedWord,
    RemovedWord,
    /// No highlight — common segment.
    None,
}

/// One sub-segment of a rendered diff line. Whole-line renders
/// produce a single segment with `WordColor::None`; word-diff lines
/// produce multiple segments with the per-word colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSegment {
    pub text: String,
    pub word_color: WordColor,
}

/// One fully rendered fallback diff line. The output of
/// [`format_diff_lines`]. Represents one row of terminal diff output,
/// without imposing a concrete rendering backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLine {
    /// Line number right-padded to `max_width`, followed by a space and the
    /// diff sigil. Drawn as non-selectable so terminal selection skips it.
    pub gutter: String,
    /// One or more text segments composing the line content. For
    /// whole-line renders this is a single `LineSegment`. For
    /// word-diff renders it's the per-word segments.
    pub content: Vec<LineSegment>,
    /// Trailing padding that fills the row out to the full terminal width:
    /// `max(0, width - used_width)` spaces.
    pub padding: String,
    /// Background color intent for this line.
    pub line_color: LineColor,
    /// Whether the consumer should render this line as dim.
    pub dim: bool,
}

/// Caller-supplied options for [`format_diff_lines`]. A struct rather than
/// positional arguments, because the pipeline takes five parameters plus the
/// two injected callbacks.
pub struct FormatOptions<'a, Wrap, WordDiff>
where
    Wrap: Fn(&str, usize) -> Vec<String>,
    WordDiff: Fn(&str, &str) -> Vec<DiffPart>,
{
    /// Terminal width in cells, clamped to at least 1.
    pub width: usize,
    /// Whether to render in dim mode. When `true`, word-level diffing is
    /// disabled.
    pub dim: bool,
    /// Line wrapping function — wraps a string to fit in `n` columns,
    /// returning each wrapped line. Injected so this module needs no
    /// particular wrapper implementation.
    pub wrap: &'a Wrap,
    /// Word-level diff function — pairs `(old, new)` strings into a sequence
    /// of common/added/removed parts. Injected so this module needs no
    /// particular differ.
    pub word_diff: &'a WordDiff,
}

// =======================
// Step 1 — transform_lines_to_objects
// =======================

/// Strips the leading `+`/`-`/space and classifies each line.
///
/// **All** branches strip the first character, so a context line loses its
/// leading space too. That is load-bearing: the renderer adds its own sigil
/// column, so the content column must not double up the prefix.
///
/// So `" hello"` becomes `code: "hello"` with type `Nochange`.
///
/// `i` is initialised to 0; [`number_diff_lines`] populates it later.
pub fn transform_lines_to_objects(lines: &[String]) -> Vec<LineObject> {
    lines
        .iter()
        .map(|line| {
            let (line_type, code) = if let Some(rest) = line.strip_prefix('+') {
                (LineType::Add, rest.to_string())
            } else if let Some(rest) = line.strip_prefix('-') {
                (LineType::Remove, rest.to_string())
            } else {
                // Every line loses its first character, context lines
                // included — strip it if there is one, otherwise leave the
                // empty string.
                let mut chars = line.chars();
                chars.next();
                (LineType::Nochange, chars.as_str().to_string())
            };
            LineObject {
                code: code.clone(),
                i: 0,
                line_type,
                original_code: code,
                word_diff: false,
                matched_line: None,
            }
        })
        .collect()
}

// =======================
// Step 2 — process_adjacent_lines
// =======================

/// Groups adjacent remove + add bursts into pairs eligible for word-level
/// diffing.
///
/// Algorithm:
///
/// 1. Walk the input linearly with index `i`.
/// 2. When the current line is `Remove`, gather all consecutive `Remove`
///    lines into `removes`, then all consecutive `Add` lines into `adds`.
/// 3. Pair them by index up to `min(removes.len(), adds.len())`. Each pair
///    gets `word_diff = true` and a `matched_line` index pointing at the
///    partner in the output vector.
/// 4. Unpaired removes and adds are still pushed, untagged.
/// 5. Anything that isn't a `Remove` is pushed as-is and `i` advances by 1.
///
/// `matched_line` holds indices into the output vector rather than references,
/// which is equivalent for the rendering pipeline below.
pub fn process_adjacent_lines(line_objects: Vec<LineObject>) -> Vec<LineObject> {
    let mut out: Vec<LineObject> = Vec::with_capacity(line_objects.len());
    let mut i = 0usize;
    while i < line_objects.len() {
        let current = &line_objects[i];
        if current.line_type != LineType::Remove {
            out.push(current.clone());
            i += 1;
            continue;
        }

        // Collect consecutive removes.
        let mut j = i;
        let mut removes_count = 0usize;
        while j < line_objects.len() && line_objects[j].line_type == LineType::Remove {
            removes_count += 1;
            j += 1;
        }

        // Collect consecutive adds following the removes.
        let adds_start = j;
        let mut adds_count = 0usize;
        while j < line_objects.len() && line_objects[j].line_type == LineType::Add {
            adds_count += 1;
            j += 1;
        }

        if removes_count > 0 && adds_count > 0 {
            // Pair them by index. The pair count is min(removes,
            // adds). Indices into the OUTPUT vector for the matched
            // partner are computed as follows:
            //   removes start at `out.len()` (we're about to push them)
            //   adds start at `out.len() + removes_count`
            let removes_out_start = out.len();
            let adds_out_start = out.len() + removes_count;
            let pair_count = removes_count.min(adds_count);

            // Push the removes (paired ones tagged with word_diff).
            for k in 0..removes_count {
                let mut line = line_objects[i + k].clone();
                if k < pair_count {
                    line.word_diff = true;
                    line.matched_line = Some(adds_out_start + k);
                }
                out.push(line);
            }
            // Push the adds (paired ones tagged with word_diff).
            for k in 0..adds_count {
                let mut line = line_objects[adds_start + k].clone();
                if k < pair_count {
                    line.word_diff = true;
                    line.matched_line = Some(removes_out_start + k);
                }
                out.push(line);
            }
            i = j;
        } else {
            // No matching adds — just push the remove (un-tagged).
            out.push(current.clone());
            i += 1;
        }
    }
    out
}

// =======================
// Step 3 — number_diff_lines
// =======================

/// Assigns 1-based line numbers to each line.
///
/// Unified-diff numbering rules:
///
/// * `Nochange` advances the counter and emits the line at the
///   current number.
/// * `Add` advances the counter and emits the line at the current
///   number.
/// * `Remove` *does not* advance the counter normally. Consecutive removes
///   are special-cased: the first remove is emitted at `i`, each following
///   remove advances `i` and is emitted at the new value, and at the end `i` is
///   **rewound** by the number of removes pushed, so the next non-remove sees
///   the original counter. Concretely, three consecutive removes starting at
///   line 10 are numbered `10, 11, 12` and the next line still starts at `10`.
///
/// This counter-rewind is the load-bearing detail — without it the
/// add lines following a remove burst would be numbered wrong.
pub fn number_diff_lines(diff: Vec<LineObject>, start_line: usize) -> Vec<LineObject> {
    let mut i = start_line;
    let mut out: Vec<LineObject> = Vec::with_capacity(diff.len());
    let mut queue: std::collections::VecDeque<LineObject> = diff.into_iter().collect();

    while let Some(current) = queue.pop_front() {
        let mut current = current;
        match current.line_type {
            LineType::Nochange => {
                current.i = i;
                i += 1;
                out.push(current);
            }
            LineType::Add => {
                current.i = i;
                i += 1;
                out.push(current);
            }
            LineType::Remove => {
                current.i = i;
                out.push(current);
                let mut num_removed = 0usize;
                // Drain consecutive removes from the queue. Each
                // consecutive remove gets a NEW line number.
                while matches!(queue.front().map(|n| n.line_type), Some(LineType::Remove)) {
                    i += 1;
                    let mut next = queue.pop_front().unwrap();
                    next.i = i;
                    out.push(next);
                    num_removed += 1;
                }
                // Rewind so the next non-remove starts at the
                // original counter.
                i -= num_removed;
            }
        }
    }

    // Indices in `matched_line` are still valid because
    // `process_adjacent_lines` produced them against the same
    // ordering and `number_diff_lines` doesn't reorder.
    out
}

// =======================
// Step 4 — decide_word_diff_path (the threshold)
// =======================

/// Decides whether a remove/add pair uses word-level diffing or falls back to
/// whole-line rendering.
///
/// Dim mode always falls back. Otherwise the ratio is
/// `changed_utf16_units / total_utf16_units` over the pair; the pair is
/// word-diffed only when that ratio is at or below [`CHANGE_THRESHOLD`] and
/// the differ returned at least one part for non-empty text (an empty result
/// with non-empty text means the differ could not answer at all).
///
/// **Edge case — empty pair.** With both sides empty the total is zero and the
/// ratio is taken as 0.0, which is below the threshold, so the pair proceeds
/// to word-level diffing (and renders nothing). The function never panics.
pub fn decide_word_diff_path(
    removed_text: &str,
    added_text: &str,
    word_diffs: &[DiffPart],
    dim: bool,
) -> WordDiffDecision {
    if dim {
        return WordDiffDecision::FallbackToWholeLine;
    }
    let total_length = removed_text.encode_utf16().count() + added_text.encode_utf16().count();
    // Empty word_diffs with non-empty text means the differ couldn't
    // produce a result (e.g. stub callback). Fall back to whole-line
    // rather than rendering empty content.
    if word_diffs.is_empty() && total_length > 0 {
        return WordDiffDecision::FallbackToWholeLine;
    }
    let changed_length: usize = word_diffs
        .iter()
        .filter(|p| matches!(p.kind, DiffPartKind::Added | DiffPartKind::Removed))
        .map(|p| p.value.encode_utf16().count())
        .sum();
    let change_ratio = if total_length == 0 {
        0.0
    } else {
        changed_length as f64 / total_length as f64
    };
    if change_ratio > CHANGE_THRESHOLD {
        WordDiffDecision::FallbackToWholeLine
    } else {
        WordDiffDecision::UseWordDiff
    }
}

// =======================
// Step 5 — format_diff_lines (the rendering pipeline)
// =======================

/// Compute the gutter+content+padding for one whole-line render.
/// Helper used by both the standard rendering path and the
/// word-diff fallback path.
fn build_gutter(line_num: Option<usize>, max_width: usize) -> String {
    let mut gutter = String::with_capacity(max_width + 1);
    if let Some(n) = line_num {
        let s = n.to_string();
        if s.len() < max_width {
            for _ in 0..(max_width - s.len()) {
                gutter.push(' ');
            }
        }
        gutter.push_str(&s);
    } else {
        for _ in 0..max_width {
            gutter.push(' ');
        }
    }
    gutter.push(' ');
    gutter
}

/// Renders the full fallback diff into backend-neutral render data.
///
/// The output is a `Vec<RenderedLine>` — one entry per *visual* line, including
/// wrapped continuations. Long source lines that don't fit
/// `available_content_width` produce multiple `RenderedLine`s, with the line
/// number drawn only on the first.
///
/// `start_line` is the source-file line number of the first line
/// (matches the caller's `patch.old_start`).
pub fn format_diff_lines<Wrap, WordDiff>(
    lines: &[String],
    start_line: usize,
    options: &FormatOptions<'_, Wrap, WordDiff>,
) -> Vec<RenderedLine>
where
    Wrap: Fn(&str, usize) -> Vec<String>,
    WordDiff: Fn(&str, &str) -> Vec<DiffPart>,
{
    // Step 0 — safe width clamp.
    let safe_width = options.width.max(1);

    // Step 1 — transform.
    let line_objects = transform_lines_to_objects(lines);
    // Step 2 — group.
    let processed = process_adjacent_lines(line_objects);
    // Step 3 — number.
    let numbered = number_diff_lines(processed, start_line);

    // Compute the max line-number width for alignment: the digits in the
    // largest line number, plus one.
    let max_line_number = numbered.iter().map(|l| l.i).max().unwrap_or(0);
    let max_width = max_line_number.to_string().len() + 1;

    let mut out: Vec<RenderedLine> = Vec::new();

    for (idx, item) in numbered.iter().enumerate() {
        // Word-level diff branch — only if the pair is tagged AND
        // the threshold check passes.
        if item.word_diff {
            if let Some(matched_idx) = item.matched_line {
                let matched = &numbered[matched_idx];
                let (removed_text, added_text) = match item.line_type {
                    LineType::Remove => {
                        (item.original_code.as_str(), matched.original_code.as_str())
                    }
                    _ => (matched.original_code.as_str(), item.original_code.as_str()),
                };
                let word_parts = (options.word_diff)(removed_text, added_text);
                if decide_word_diff_path(removed_text, added_text, &word_parts, options.dim)
                    == WordDiffDecision::UseWordDiff
                {
                    let rendered = render_word_diff_line(
                        item,
                        idx,
                        &word_parts,
                        safe_width,
                        max_width,
                        options.dim,
                        options.wrap,
                    );
                    out.extend(rendered);
                    continue;
                }
            }
        }

        // Whole-line render — the standard path.
        let rendered = render_whole_line(item, safe_width, max_width, options.dim, options.wrap);
        out.extend(rendered);
    }

    out
}

fn render_whole_line<Wrap>(
    item: &LineObject,
    safe_width: usize,
    max_width: usize,
    dim: bool,
    wrap: &Wrap,
) -> Vec<RenderedLine>
where
    Wrap: Fn(&str, usize) -> Vec<String>,
{
    // diff-prefix-width is 2 in the standard branch ("  " or "+ "
    // or "- "). The available content width subtracts the line-num
    // column, the space after it, and the prefix width.
    let diff_prefix_width = 2usize;
    let available_content_width = safe_width
        .saturating_sub(max_width)
        .saturating_sub(1)
        .saturating_sub(diff_prefix_width)
        .max(1);

    let wrapped = wrap(&item.code, available_content_width);
    let wrapped = if wrapped.is_empty() {
        vec![String::new()]
    } else {
        wrapped
    };

    let sigil = match item.line_type {
        LineType::Add => '+',
        LineType::Remove => '-',
        LineType::Nochange => ' ',
    };

    let line_color = match (item.line_type, dim) {
        (LineType::Add, false) => LineColor::Added,
        (LineType::Add, true) => LineColor::AddedDimmed,
        (LineType::Remove, false) => LineColor::Removed,
        (LineType::Remove, true) => LineColor::RemovedDimmed,
        (LineType::Nochange, _) => LineColor::None,
    };

    wrapped
        .into_iter()
        .enumerate()
        .map(|(line_index, line_text)| {
            let line_num = if line_index == 0 { Some(item.i) } else { None };
            let mut gutter = build_gutter(line_num, max_width);
            gutter.push(sigil);

            let content_width = WidthStr::width(line_text.as_str());
            // Used width = gutter (max_width chars + 1 space) + sigil (1) + content
            let used_width = max_width + 1 + 1 + content_width;
            let padding_count = safe_width.saturating_sub(used_width);
            let padding = " ".repeat(padding_count);

            RenderedLine {
                gutter,
                content: vec![LineSegment {
                    text: line_text,
                    word_color: WordColor::None,
                }],
                padding,
                line_color,
                dim: dim || matches!(item.line_type, LineType::Nochange),
            }
        })
        .collect()
}

fn render_word_diff_line<Wrap>(
    item: &LineObject,
    _output_index: usize,
    word_parts: &[DiffPart],
    safe_width: usize,
    max_width: usize,
    dim: bool,
    wrap: &Wrap,
) -> Vec<RenderedLine>
where
    Wrap: Fn(&str, usize) -> Vec<String>,
{
    // Word-diff branch uses a prefix width of 1 — just '+' or '-', with no
    // following space, unlike the space-padded sigil of the standard branch.
    let diff_prefix = match item.line_type {
        LineType::Add => '+',
        LineType::Remove => '-',
        LineType::Nochange => ' ', // Defensive — word-diff is only set on add/remove
    };
    let diff_prefix_width = 1usize;
    let available_content_width = safe_width
        .saturating_sub(max_width)
        .saturating_sub(1)
        .saturating_sub(diff_prefix_width)
        .max(1);

    // Walk parts. For each part, decide if it should be rendered
    // for the current line side.
    let mut wrapped_lines: Vec<(Vec<LineSegment>, usize)> = Vec::new();
    let mut current_line: Vec<LineSegment> = Vec::new();
    let mut current_width: usize = 0;

    for part in word_parts {
        let (should_show, word_color) = match item.line_type {
            LineType::Add => match part.kind {
                DiffPartKind::Added => (true, WordColor::AddedWord),
                DiffPartKind::Common => (true, WordColor::None),
                DiffPartKind::Removed => (false, WordColor::None),
            },
            LineType::Remove => match part.kind {
                DiffPartKind::Removed => (true, WordColor::RemovedWord),
                DiffPartKind::Common => (true, WordColor::None),
                DiffPartKind::Added => (false, WordColor::None),
            },
            LineType::Nochange => (false, WordColor::None),
        };
        if !should_show {
            continue;
        }

        // Wrap this part to the available width.
        let part_lines = wrap(&part.value, available_content_width);
        let part_lines = if part_lines.is_empty() {
            vec![String::new()]
        } else {
            part_lines
        };
        for (line_idx, part_line) in part_lines.iter().enumerate() {
            if part_line.is_empty() {
                // Skip empty lines coming out of the wrapper.
                continue;
            }
            // Start a new line if (a) we're past the first part-line
            // OR (b) appending would overflow the width.
            let part_line_width = WidthStr::width(part_line.as_str());
            if line_idx > 0 || current_width + part_line_width > available_content_width {
                if !current_line.is_empty() {
                    wrapped_lines.push((std::mem::take(&mut current_line), current_width));
                    current_width = 0;
                }
            }
            current_line.push(LineSegment {
                text: part_line.clone(),
                word_color,
            });
            current_width += part_line_width;
        }
    }
    if !current_line.is_empty() {
        wrapped_lines.push((current_line, current_width));
    }
    if wrapped_lines.is_empty() {
        // Defensive — produce at least one (empty) line so the gutter still
        // renders and the line number appears in the output.
        wrapped_lines.push((Vec::new(), 0));
    }

    let line_color = match (item.line_type, dim) {
        (LineType::Add, false) => LineColor::Added,
        (LineType::Add, true) => LineColor::AddedDimmed,
        (LineType::Remove, false) => LineColor::Removed,
        (LineType::Remove, true) => LineColor::RemovedDimmed,
        (LineType::Nochange, _) => LineColor::None,
    };

    wrapped_lines
        .into_iter()
        .enumerate()
        .map(|(line_index, (segments, content_width))| {
            let line_num = if line_index == 0 { Some(item.i) } else { None };
            let mut gutter = build_gutter(line_num, max_width);
            gutter.push(diff_prefix);

            // Used width = gutter (max_width + 1) + sigil (1) + content
            let used_width = max_width + 1 + diff_prefix_width + content_width;
            let padding_count = safe_width.saturating_sub(used_width);
            let padding = " ".repeat(padding_count);

            RenderedLine {
                gutter,
                content: segments,
                padding,
                line_color,
                dim,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test wrapper: splits on display width, which is good enough for the
    /// ASCII fixtures. The real, ANSI-aware wrapper is injected by the caller.
    fn naive_wrap(text: &str, width: usize) -> Vec<String> {
        if width == 0 || text.is_empty() {
            return vec![text.to_string()];
        }
        let mut out = Vec::new();
        let mut buf = String::new();
        let mut buf_width = 0usize;
        for ch in text.chars() {
            let w = WidthStr::width(ch.to_string().as_str());
            if buf_width + w > width && !buf.is_empty() {
                out.push(std::mem::take(&mut buf));
                buf_width = 0;
            }
            buf.push(ch);
            buf_width += w;
        }
        if !buf.is_empty() {
            out.push(buf);
        }
        if out.is_empty() {
            vec![String::new()]
        } else {
            out
        }
    }

    /// Test word-differ: any difference becomes one removed part followed by one
    /// added part. Deliberately simple — the real differ is injected.
    fn fake_word_diff(old: &str, new: &str) -> Vec<DiffPart> {
        if old == new {
            return vec![DiffPart::common(old)];
        }
        // Naive: emit `Removed(old)` then `Added(new)`. Not word-aware, but
        // stable, and it lets the threshold tests pin specific change ratios.
        vec![DiffPart::removed(old), DiffPart::added(new)]
    }

    /// Differ used by the word-diff rendering tests, wrapping
    /// [`calculate_word_diff`] so real common-then-changed shapes come out.
    fn structured_word_diff(old: &str, new: &str) -> Vec<DiffPart> {
        calculate_word_diff(old, new)
    }

    #[test]
    fn calculate_word_diff_preserves_unicode_context() {
        assert_eq!(
            calculate_word_diff(
                "    (\"claude-code-guide\", \"文档问答\"),",
                "    (\"rebon-code-guide\", \"文档问答\"),",
            ),
            vec![
                DiffPart::common("    (\""),
                DiffPart::removed("claude"),
                DiffPart::added("rebon"),
                DiffPart::common("-code-guide\", \"文档问答\"),"),
            ]
        );
    }

    // =======================
    // transform_lines_to_objects
    // =======================

    #[test]
    fn transform_classifies_add_lines() {
        let lines = vec!["+added line".to_string()];
        let out = transform_lines_to_objects(&lines);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line_type, LineType::Add);
        assert_eq!(out[0].code, "added line");
        assert_eq!(out[0].original_code, "added line");
        assert_eq!(out[0].i, 0);
        assert!(!out[0].word_diff);
        assert!(out[0].matched_line.is_none());
    }

    #[test]
    fn transform_classifies_remove_lines() {
        let lines = vec!["-removed line".to_string()];
        let out = transform_lines_to_objects(&lines);
        assert_eq!(out[0].line_type, LineType::Remove);
        assert_eq!(out[0].code, "removed line");
    }

    #[test]
    fn transform_classifies_nochange_lines() {
        // " context line" → strip first char (the space).
        let lines = vec![" context line".to_string()];
        let out = transform_lines_to_objects(&lines);
        assert_eq!(out[0].line_type, LineType::Nochange);
        assert_eq!(out[0].code, "context line");
    }

    #[test]
    fn transform_strips_one_character_for_each_branch() {
        // The load-bearing detail: every branch strips the first character,
        // not just add/remove. Verified by mixed input.
        let lines = vec![
            "+a".to_string(),
            "-b".to_string(),
            " c".to_string(),
            "+++".to_string(), // strip leading +, leaves "++"
            "-".to_string(),   // strip leading -, leaves ""
        ];
        let out = transform_lines_to_objects(&lines);
        assert_eq!(out[0].code, "a");
        assert_eq!(out[1].code, "b");
        assert_eq!(out[2].code, "c");
        assert_eq!(out[3].code, "++");
        assert_eq!(out[4].code, "");
    }

    #[test]
    fn transform_handles_empty_input() {
        let out = transform_lines_to_objects(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn transform_empty_string_becomes_empty_nochange() {
        // Edge case: the empty string has no first character to strip, so the
        // result is a `Nochange` line with empty code.
        let lines = vec!["".to_string()];
        let out = transform_lines_to_objects(&lines);
        assert_eq!(out[0].line_type, LineType::Nochange);
        assert_eq!(out[0].code, "");
    }

    // =======================
    // process_adjacent_lines
    // =======================

    fn obj(line_type: LineType, code: &str) -> LineObject {
        LineObject {
            code: code.to_string(),
            i: 0,
            line_type,
            original_code: code.to_string(),
            word_diff: false,
            matched_line: None,
        }
    }

    #[test]
    fn process_no_changes_passes_through() {
        let input = vec![obj(LineType::Nochange, "a"), obj(LineType::Nochange, "b")];
        let out = process_adjacent_lines(input);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|l| !l.word_diff));
    }

    #[test]
    fn process_pairs_one_remove_one_add() {
        let input = vec![obj(LineType::Remove, "old"), obj(LineType::Add, "new")];
        let out = process_adjacent_lines(input);
        assert_eq!(out.len(), 2);
        assert!(out[0].word_diff, "remove must be tagged");
        assert!(out[1].word_diff, "add must be tagged");
        assert_eq!(out[0].matched_line, Some(1));
        assert_eq!(out[1].matched_line, Some(0));
    }

    #[test]
    fn process_pairs_two_removes_two_adds() {
        let input = vec![
            obj(LineType::Remove, "r1"),
            obj(LineType::Remove, "r2"),
            obj(LineType::Add, "a1"),
            obj(LineType::Add, "a2"),
        ];
        let out = process_adjacent_lines(input);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].matched_line, Some(2));
        assert_eq!(out[1].matched_line, Some(3));
        assert_eq!(out[2].matched_line, Some(0));
        assert_eq!(out[3].matched_line, Some(1));
        assert!(out.iter().all(|l| l.word_diff));
    }

    #[test]
    fn process_unpaired_excess_removes_are_untagged() {
        let input = vec![
            obj(LineType::Remove, "r1"),
            obj(LineType::Remove, "r2"),
            obj(LineType::Remove, "r3"),
            obj(LineType::Add, "a1"),
        ];
        let out = process_adjacent_lines(input);
        assert_eq!(out.len(), 4);
        // r1 and a1 should be paired; r2 and r3 unpaired.
        assert!(out[0].word_diff);
        assert_eq!(out[0].matched_line, Some(3));
        assert!(!out[1].word_diff, "r2 unpaired");
        assert!(!out[2].word_diff, "r3 unpaired");
        assert!(out[3].word_diff);
        assert_eq!(out[3].matched_line, Some(0));
    }

    #[test]
    fn process_unpaired_excess_adds_are_untagged() {
        let input = vec![
            obj(LineType::Remove, "r1"),
            obj(LineType::Add, "a1"),
            obj(LineType::Add, "a2"),
        ];
        let out = process_adjacent_lines(input);
        assert!(out[0].word_diff);
        assert!(out[1].word_diff);
        assert_eq!(out[1].matched_line, Some(0));
        assert!(!out[2].word_diff, "a2 unpaired");
    }

    #[test]
    fn process_remove_with_no_following_add_is_not_tagged() {
        let input = vec![obj(LineType::Remove, "r1"), obj(LineType::Nochange, "ctx")];
        let out = process_adjacent_lines(input);
        assert!(!out[0].word_diff);
        assert!(!out[1].word_diff);
    }

    #[test]
    fn process_lone_add_is_not_tagged() {
        let input = vec![obj(LineType::Add, "a1")];
        let out = process_adjacent_lines(input);
        assert!(!out[0].word_diff);
    }

    #[test]
    fn process_multiple_distinct_pair_groups() {
        let input = vec![
            obj(LineType::Nochange, "ctx1"),
            obj(LineType::Remove, "r1"),
            obj(LineType::Add, "a1"),
            obj(LineType::Nochange, "ctx2"),
            obj(LineType::Remove, "r2"),
            obj(LineType::Add, "a2"),
        ];
        let out = process_adjacent_lines(input);
        assert_eq!(out.len(), 6);
        assert!(!out[0].word_diff);
        assert!(out[1].word_diff);
        assert_eq!(out[1].matched_line, Some(2));
        assert!(out[2].word_diff);
        assert_eq!(out[2].matched_line, Some(1));
        assert!(!out[3].word_diff);
        assert!(out[4].word_diff);
        assert_eq!(out[4].matched_line, Some(5));
        assert!(out[5].word_diff);
        assert_eq!(out[5].matched_line, Some(4));
    }

    #[test]
    fn process_empty_input_returns_empty() {
        let out = process_adjacent_lines(vec![]);
        assert!(out.is_empty());
    }

    // =======================
    // number_diff_lines
    // =======================

    #[test]
    fn number_pure_nochange_advances_normally() {
        let input = vec![
            obj(LineType::Nochange, "a"),
            obj(LineType::Nochange, "b"),
            obj(LineType::Nochange, "c"),
        ];
        let out = number_diff_lines(input, 10);
        assert_eq!(out[0].i, 10);
        assert_eq!(out[1].i, 11);
        assert_eq!(out[2].i, 12);
    }

    #[test]
    fn number_pure_adds_advance_normally() {
        let input = vec![
            obj(LineType::Add, "a"),
            obj(LineType::Add, "b"),
            obj(LineType::Add, "c"),
        ];
        let out = number_diff_lines(input, 5);
        assert_eq!(out[0].i, 5);
        assert_eq!(out[1].i, 6);
        assert_eq!(out[2].i, 7);
    }

    #[test]
    fn number_remove_burst_then_nochange_rewinds() {
        // Three removes starting at line 10: numbered 10, 11, 12.
        // The next nochange should be at line 10 again (rewind).
        let input = vec![
            obj(LineType::Remove, "r1"),
            obj(LineType::Remove, "r2"),
            obj(LineType::Remove, "r3"),
            obj(LineType::Nochange, "ctx"),
        ];
        let out = number_diff_lines(input, 10);
        assert_eq!(out[0].i, 10);
        assert_eq!(out[1].i, 11);
        assert_eq!(out[2].i, 12);
        // Rewind: i -= num_removed (= 2 for the loop body), then the nochange
        // branch advances by 1 → 10 + 1 = 11.
        // Concretely: i was 10 at start, hit remove at 10, looped:
        //   iter1: i=11, push, num_removed=1
        //   iter2: i=12, push, num_removed=2
        // Then i -= 2 → i=10. Nochange: out at 10, i++ → 11.
        assert_eq!(out[3].i, 10);
    }

    #[test]
    fn number_single_remove_then_nochange_does_not_advance() {
        let input = vec![obj(LineType::Remove, "r1"), obj(LineType::Nochange, "ctx")];
        let out = number_diff_lines(input, 10);
        assert_eq!(out[0].i, 10);
        // Single remove, no inner loop iterations, num_removed=0.
        // i stays at 10. Nochange emits at 10, then i++ → 11.
        assert_eq!(out[1].i, 10);
    }

    #[test]
    fn number_remove_followed_by_add_pair() {
        // Classic remove/add pair. Remove at 10, add at 10.
        let input = vec![obj(LineType::Remove, "old"), obj(LineType::Add, "new")];
        let out = number_diff_lines(input, 10);
        assert_eq!(out[0].i, 10);
        assert_eq!(out[1].i, 10);
    }

    #[test]
    fn number_two_removes_then_two_adds() {
        // Removes: r1@10, r2@11. After rewind, i=10.
        // Adds: a1@10 (i=11), a2@11 (i=12).
        let input = vec![
            obj(LineType::Remove, "r1"),
            obj(LineType::Remove, "r2"),
            obj(LineType::Add, "a1"),
            obj(LineType::Add, "a2"),
        ];
        let out = number_diff_lines(input, 10);
        assert_eq!(out[0].i, 10);
        assert_eq!(out[1].i, 11);
        assert_eq!(out[2].i, 10);
        assert_eq!(out[3].i, 11);
    }

    #[test]
    fn number_starts_at_arbitrary_offset() {
        let input = vec![obj(LineType::Nochange, "a"), obj(LineType::Nochange, "b")];
        let out = number_diff_lines(input, 1000);
        assert_eq!(out[0].i, 1000);
        assert_eq!(out[1].i, 1001);
    }

    #[test]
    fn number_empty_input_returns_empty() {
        let out = number_diff_lines(vec![], 1);
        assert!(out.is_empty());
    }

    // =======================
    // decide_word_diff_path
    // =======================

    #[test]
    fn dim_mode_always_falls_back() {
        let parts = vec![DiffPart::common("hi")];
        assert_eq!(
            decide_word_diff_path("hi", "hi", &parts, true),
            WordDiffDecision::FallbackToWholeLine
        );
    }

    #[test]
    fn no_changes_proceeds_with_word_diff() {
        // Identical inputs → 0 / total = 0, ratio 0.0, threshold 0.4
        // → UseWordDiff.
        let parts = vec![DiffPart::common("hello world")];
        assert_eq!(
            decide_word_diff_path("hello world", "hello world", &parts, false),
            WordDiffDecision::UseWordDiff
        );
    }

    #[test]
    fn small_change_under_threshold_uses_word_diff() {
        // Change ratio: 4 / (15 + 15) = 0.13 → under 0.4
        let parts = vec![
            DiffPart::common("function ("),
            DiffPart::removed("foo"),
            DiffPart::added("bar"),
            DiffPart::common(") { return; }"),
        ];
        // total = 15 + 15 = 30, changed = 3 + 3 = 6, ratio = 0.2
        assert_eq!(
            decide_word_diff_path(
                "function (foo) { return; }",
                "function (bar) { return; }",
                &parts,
                false
            ),
            WordDiffDecision::UseWordDiff
        );
    }

    #[test]
    fn large_change_over_threshold_falls_back() {
        // Total = 5 + 5 = 10. Changed = 5 + 5 = 10. Ratio = 1.0 >> 0.4.
        let parts = vec![DiffPart::removed("aaaaa"), DiffPart::added("bbbbb")];
        assert_eq!(
            decide_word_diff_path("aaaaa", "bbbbb", &parts, false),
            WordDiffDecision::FallbackToWholeLine
        );
    }

    #[test]
    fn exactly_at_threshold_uses_word_diff() {
        // Ratio = exactly 0.4 → `> 0.4` is false → UseWordDiff.
        // Construct parts with changed=4, total=10.
        let parts = vec![
            DiffPart::common("123"),
            DiffPart::removed("ab"),
            DiffPart::added("cd"),
        ];
        // total = 5 + 5 = 10, changed = 2 + 2 = 4, ratio = 0.4
        assert_eq!(
            decide_word_diff_path("123ab", "123cd", &parts, false),
            WordDiffDecision::UseWordDiff
        );
    }

    #[test]
    fn just_over_threshold_falls_back() {
        // Ratio just over 0.4. total=10, changed=5 → 0.5.
        let parts = vec![
            DiffPart::common("12"),
            DiffPart::removed("abc"),
            DiffPart::added("xyz"),
        ];
        assert_eq!(
            decide_word_diff_path("12abc", "12xyz", &parts, false),
            WordDiffDecision::FallbackToWholeLine
        );
    }

    #[test]
    fn empty_inputs_do_not_panic_division_by_zero() {
        // total = 0, changed = 0 → ratio = 0.0 (no NaN here).
        // 0.0 > 0.4 is false → UseWordDiff.
        let parts: Vec<DiffPart> = vec![];
        assert_eq!(
            decide_word_diff_path("", "", &parts, false),
            WordDiffDecision::UseWordDiff
        );
    }

    #[test]
    fn utf16_code_units_drive_change_ratio() {
        // Lengths are counted in UTF-16 code units, not UTF-8 bytes. With
        // unchanged CJK in the common prefix, counting bytes would make the
        // ratio look much smaller.
        // removed: 你你ab → 4 code units
        // added:   你你cd → 4 code units
        // total = 8, changed = 2 + 2 = 4, ratio = 0.5 -> fallback.
        let parts = vec![
            DiffPart::common("你你"),
            DiffPart::removed("ab"),
            DiffPart::added("cd"),
        ];
        assert_eq!(
            decide_word_diff_path("你你ab", "你你cd", &parts, false),
            WordDiffDecision::FallbackToWholeLine
        );
    }

    #[test]
    fn dim_overrides_even_low_change_ratio() {
        let parts = vec![DiffPart::common("hi")];
        assert_eq!(
            decide_word_diff_path("hi", "hi", &parts, true),
            WordDiffDecision::FallbackToWholeLine
        );
    }

    #[test]
    fn empty_parts_with_content_falls_back() {
        // When the word-differ returns nothing but the texts are non-empty,
        // fall back to whole-line rendering to avoid blank content.
        let parts: Vec<DiffPart> = vec![];
        assert_eq!(
            decide_word_diff_path("hello", "world", &parts, false),
            WordDiffDecision::FallbackToWholeLine
        );
    }

    // =======================
    // build_gutter helper
    // =======================

    #[test]
    fn gutter_with_line_number_right_pads() {
        // line=5, max_width=3 → "  5 " (2 spaces, '5', 1 trailing space)
        let g = build_gutter(Some(5), 3);
        assert_eq!(g, "  5 ");
    }

    #[test]
    fn gutter_with_no_line_number_uses_blanks() {
        let g = build_gutter(None, 3);
        assert_eq!(g, "    ");
        assert_eq!(g.len(), 4); // 3 blanks + 1 trailing space
    }

    #[test]
    fn gutter_line_number_wider_than_max_width() {
        // line=100, max_width=2 → "100 " (no left-pad needed)
        let g = build_gutter(Some(100), 2);
        assert_eq!(g, "100 ");
    }

    // =======================
    // format_diff_lines — full pipeline integration
    // =======================

    #[test]
    fn format_pure_nochange_diff() {
        let lines = vec![" context".to_string()];
        let opts = FormatOptions {
            width: 40,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 5, &opts);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line_color, LineColor::None);
        assert!(out[0].dim, "nochange lines are always rendered dim");
        assert_eq!(out[0].content[0].text, "context");
    }

    #[test]
    fn format_pure_added_line() {
        let lines = vec!["+added line".to_string()];
        let opts = FormatOptions {
            width: 40,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line_color, LineColor::Added);
        assert!(!out[0].dim);
        // Gutter shape: "1 +" (max_width = digits(1)+1 = 2; "1" → " 1 " then '+')
        assert!(out[0].gutter.ends_with('+'));
    }

    #[test]
    fn format_pure_removed_line() {
        let lines = vec!["-removed".to_string()];
        let opts = FormatOptions {
            width: 40,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        assert_eq!(out[0].line_color, LineColor::Removed);
        assert!(out[0].gutter.ends_with('-'));
    }

    #[test]
    fn format_dim_uses_dimmed_line_colors() {
        let lines = vec!["+a".to_string(), "-b".to_string()];
        let opts = FormatOptions {
            width: 40,
            dim: true,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        // Note: even with the remove+add pair, dim disables word-diff,
        // so both lines render through the standard branch.
        assert_eq!(out[0].line_color, LineColor::AddedDimmed);
        assert_eq!(out[1].line_color, LineColor::RemovedDimmed);
        assert!(out[0].dim);
        assert!(out[1].dim);
    }

    #[test]
    fn format_word_diff_pair_uses_word_diff_branch() {
        // Simulate "function foo()" → "function bar()" (3-char delta).
        let lines = vec!["-function foo()".to_string(), "+function bar()".to_string()];
        let opts = FormatOptions {
            width: 40,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &structured_word_diff,
        };
        let out = format_diff_lines(&lines, 10, &opts);
        // Two visual lines (one per source line; both fit). Each
        // should have multiple segments (common + highlighted word).
        assert_eq!(out.len(), 2);
        assert!(
            out[0].content.len() > 1,
            "remove line should have multiple segments after word-diff"
        );
        // Find the highlighted segment in the remove line.
        let removed_segment = out[0]
            .content
            .iter()
            .find(|s| s.word_color == WordColor::RemovedWord);
        assert!(removed_segment.is_some(), "must contain a Removed segment");
        // Find the highlighted segment in the add line.
        let added_segment = out[1]
            .content
            .iter()
            .find(|s| s.word_color == WordColor::AddedWord);
        assert!(added_segment.is_some(), "must contain an Added segment");
    }

    #[test]
    fn format_word_diff_falls_back_when_change_too_large() {
        // Two completely different lines → 100% change → fall back.
        let lines = vec!["-aaaaaa".to_string(), "+bbbbbb".to_string()];
        let opts = FormatOptions {
            width: 40,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        // Both lines render through the standard branch (single
        // segment, WordColor::None).
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content.len(), 1);
        assert_eq!(out[0].content[0].word_color, WordColor::None);
        assert_eq!(out[1].content.len(), 1);
        assert_eq!(out[1].content[0].word_color, WordColor::None);
    }

    #[test]
    fn format_long_line_wraps_and_only_first_has_line_number() {
        // Force wrapping by giving a very narrow width.
        let lines = vec!["+aaaaaaaaaaaaaaa".to_string()];
        let opts = FormatOptions {
            width: 10,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        // Wrapped — multiple visual lines.
        assert!(out.len() > 1, "long line must wrap into multiple lines");
        // First line: gutter contains "1"
        assert!(
            out[0].gutter.trim_end().ends_with("1 +") || out[0].gutter.contains('1'),
            "first line gutter should carry line number, got {:?}",
            out[0].gutter
        );
        // Continuation: gutter has only blanks + sigil
        let cont_gutter = &out[1].gutter;
        let cont_trimmed = cont_gutter.trim_end_matches('+');
        assert!(
            cont_trimmed.chars().all(|c| c == ' '),
            "continuation gutter should be blank, got {cont_gutter:?}"
        );
    }

    #[test]
    fn format_padding_fills_to_full_width() {
        let lines = vec!["+ab".to_string()];
        let opts = FormatOptions {
            width: 20,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        let row = &out[0];
        let total = row.gutter.len() + row.content[0].text.len() + row.padding.len();
        assert_eq!(total, 20);
    }

    #[test]
    fn format_padding_one_when_content_fills_available_column() {
        // The available-width calculation reserves a two-cell diff prefix, but
        // the gutter emits only a one-cell sigil. So even when content fills
        // the available column exactly, padding is at least one cell — the
        // unused half of the two-cell reservation.
        //
        // Trace for width=10, line "+abcde":
        //   max_width = digits(1) + 1 = 2
        //   available_content_width = 10 - 2 - 1 - 2 = 5
        //   wrapped("abcde", 5) = ["abcde"] (one line)
        //   used_width = 2 + 1 + 1 + 5 = 9
        //   padding = 10 - 9 = 1
        let lines = vec!["+abcde".to_string()];
        let opts = FormatOptions {
            width: 10,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        assert_eq!(out.len(), 1);
        let row = &out[0];
        // Total visual width = gutter (4) + content (5) + padding (1) = 10
        let total = row.gutter.len()
            + row.content.iter().map(|s| s.text.len()).sum::<usize>()
            + row.padding.len();
        assert_eq!(total, 10);
        assert_eq!(row.padding, " ");
    }

    #[test]
    fn format_empty_input_returns_empty() {
        let opts = FormatOptions {
            width: 40,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&[], 1, &opts);
        assert!(out.is_empty());
    }

    #[test]
    fn format_width_one_does_not_panic_or_underflow() {
        // Stress: width = 1 makes available_content_width saturate to 1.
        // No panic.
        let lines = vec!["+a".to_string()];
        let opts = FormatOptions {
            width: 1,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &fake_word_diff,
        };
        let out = format_diff_lines(&lines, 1, &opts);
        assert!(!out.is_empty());
    }

    #[test]
    fn format_realistic_three_line_diff() {
        let lines = vec![
            " context line".to_string(),
            "-old code".to_string(),
            "+new code".to_string(),
            " trailing".to_string(),
        ];
        let opts = FormatOptions {
            width: 40,
            dim: false,
            wrap: &naive_wrap,
            word_diff: &structured_word_diff,
        };
        let out = format_diff_lines(&lines, 10, &opts);
        // 4 source lines → at least 4 visual lines.
        assert!(out.len() >= 4);
        // First and last are nochange.
        assert_eq!(out[0].line_color, LineColor::None);
        assert_eq!(out.last().unwrap().line_color, LineColor::None);
    }

    // =======================
    // Constant pinning
    // =======================

    #[test]
    fn change_threshold_is_pinned() {
        // Pinned exactly.
        assert_eq!(CHANGE_THRESHOLD, 0.4);
    }

    // =======================
    // Exhaustive table — line-classification round-trips
    // =======================

    #[test]
    fn line_classification_table() {
        // (raw line input, expected type, expected stripped code)
        let cases: &[(&str, LineType, &str)] = &[
            ("+added", LineType::Add, "added"),
            ("-removed", LineType::Remove, "removed"),
            (" context", LineType::Nochange, "context"),
            ("+", LineType::Add, ""),
            ("-", LineType::Remove, ""),
            (" ", LineType::Nochange, ""),
            ("", LineType::Nochange, ""),
            ("++", LineType::Add, "+"),
            ("--", LineType::Remove, "-"),
            ("  double space", LineType::Nochange, " double space"),
            ("+   leading spaces", LineType::Add, "   leading spaces"),
        ];
        for (raw, expected_type, expected_code) in cases.iter().copied() {
            let out = transform_lines_to_objects(&[raw.to_string()]);
            assert_eq!(
                out[0].line_type, expected_type,
                "wrong type for input {raw:?}"
            );
            assert_eq!(out[0].code, expected_code, "wrong code for input {raw:?}");
        }
    }
}
