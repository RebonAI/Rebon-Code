//! Edit/Write diff computation: turn a tool's before/after text into `+`/`-`/` `
//! prefixed marker lines, with optional surrounding file context.
//!
//! Everything here is pure `std`: measuring and painting a diff block are the
//! consumer's job, so every surface renders the same diff from one
//! computation.

use std::borrow::Cow;

/// Generate `+`/`-` prefixed diff lines from old and new text.
///
/// This produces a simple replacement diff (all old lines removed, all new
/// lines added) with no interleaving — kept deliberately for the
/// [`diff_summary`] line counts and the single-line tool-body summary, which
/// want the raw added/removed totals. For the rendered diff *body* (which
/// benefits from seeing each change next to its surroundings) use
/// [`interleaved_diff_lines`] / [`build_context_diff`] instead.
pub fn generate_diff_lines(old_text: Option<&str>, new_text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(old) = old_text {
        for line in old.lines() {
            lines.push(format!("-{line}"));
        }
        // Handle trailing newline edge case: if old text is empty,
        // add a single removal marker.
        if old.is_empty() {
            lines.push("-".into());
        }
    }
    for line in new_text.lines() {
        lines.push(format!("+{line}"));
    }
    if new_text.is_empty() {
        lines.push("+".into());
    }
    lines
}

/// A single line-level operation produced by [`lcs_diff_ops`].
pub enum DiffOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// Guard against the O(n*m) time/space of the LCS table: above this
/// product of line counts we degrade to the all-removed-then-all-added
/// shape rather than allocating a huge table for a pathological edit.
pub const LCS_DIFF_BUDGET: usize = 4_000_000;

const DIFF_COUNT_MAX_EDIT_DISTANCE: usize = 1_024;
const DIFF_COUNT_WORK_BUDGET: usize = 1_000_000;

/// Count line insertions and deletions without materializing rendered diff
/// lines. Sparse edits use a bounded Myers pass; pathological inputs fall back
/// to the changed core after common leading and trailing lines are removed.
pub fn diff_line_counts(old_text: Option<&str>, new_text: &str) -> (usize, usize) {
    let Some(old_text) = old_text else {
        return (new_text.lines().count().max(1), 0);
    };
    if old_text == new_text {
        return (0, 0);
    }

    let old_lines = old_text.lines().collect::<Vec<_>>();
    let new_lines = new_text.lines().collect::<Vec<_>>();
    let mut leading = 0;
    let common_len = old_lines.len().min(new_lines.len());
    while leading < common_len && old_lines[leading] == new_lines[leading] {
        leading += 1;
    }

    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > leading && new_end > leading && old_lines[old_end - 1] == new_lines[new_end - 1]
    {
        old_end -= 1;
        new_end -= 1;
    }

    let old_core = &old_lines[leading..old_end];
    let new_core = &new_lines[leading..new_end];
    if old_core.is_empty() {
        return (new_core.len(), 0);
    }
    if new_core.is_empty() {
        return (0, old_core.len());
    }

    let Some(distance) = myers_edit_distance(old_core, new_core) else {
        return (new_core.len(), old_core.len());
    };
    let common = (old_core.len() + new_core.len() - distance) / 2;
    (new_core.len() - common, old_core.len() - common)
}

fn myers_edit_distance(old: &[&str], new: &[&str]) -> Option<usize> {
    let max_distance = old.len().saturating_add(new.len());
    let distance_limit = max_distance.min(DIFF_COUNT_MAX_EDIT_DISTANCE);
    if old.len().abs_diff(new.len()) > distance_limit {
        return None;
    }

    let offset = distance_limit + 1;
    let mut furthest = vec![0isize; distance_limit * 2 + 3];
    let old_len = old.len() as isize;
    let new_len = new.len() as isize;
    let mut work = 0usize;

    for distance in 0..=distance_limit {
        let distance = distance as isize;
        for diagonal in (-distance..=distance).step_by(2) {
            work += 1;
            if work > DIFF_COUNT_WORK_BUDGET {
                return None;
            }

            let index = (offset as isize + diagonal) as usize;
            let mut x = if diagonal == -distance
                || (diagonal != distance && furthest[index - 1] < furthest[index + 1])
            {
                furthest[index + 1]
            } else {
                furthest[index - 1] + 1
            };
            let mut y = x - diagonal;
            while x < old_len && y < new_len && old[x as usize] == new[y as usize] {
                work += 1;
                if work > DIFF_COUNT_WORK_BUDGET {
                    return None;
                }
                x += 1;
                y += 1;
            }
            furthest[index] = x;
            if x >= old_len && y >= new_len {
                return Some(distance as usize);
            }
        }
    }

    None
}

/// Line-level longest-common-subsequence diff. Returns interleaved
/// `Equal`/`Delete`/`Insert` ops so each change sits next to the
/// unchanged lines around it — the shape a real diff tool produces,
/// rather than `generate_diff_lines`' all-removed-then-all-added shape.
pub fn lcs_diff_ops<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<DiffOp<'a>> {
    let n = old_lines.len();
    let m = new_lines.len();
    // dp[i][j] = LCS length of old_lines[i..] vs new_lines[j..].
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    // Backtrack from the top-left, preferring deletions on ties so an
    // edit that replaces a block reads "old then new" within each hunk.
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            ops.push(DiffOp::Equal(old_lines[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(DiffOp::Delete(old_lines[i]));
            i += 1;
        } else {
            ops.push(DiffOp::Insert(new_lines[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffOp::Delete(old_lines[i]));
        i += 1;
    }
    while j < m {
        ops.push(DiffOp::Insert(new_lines[j]));
        j += 1;
    }
    ops
}

/// Generate interleaved ` `/`+`/`-` prefixed change lines between
/// `old_text` and `new_text` via a line-level LCS pass. Unlike
/// [`generate_diff_lines`] (all removals, then all additions), each
/// change sits next to the unchanged lines around it, so a
/// head-truncated preview surfaces both what was removed and what it
/// became. No surrounding file context is added here — that is
/// [`build_context_diff`]'s job. Degrades to the all-removed-then-added
/// shape when the LCS table would exceed [`LCS_DIFF_BUDGET`].
pub fn interleaved_diff_lines(old_text: &str, new_text: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    if old_lines.is_empty() {
        return new_lines.iter().map(|l| format!("+{l}")).collect();
    }
    if new_lines.is_empty() {
        return old_lines.iter().map(|l| format!("-{l}")).collect();
    }
    if old_lines.len().saturating_mul(new_lines.len()) > LCS_DIFF_BUDGET {
        let mut lines = Vec::with_capacity(old_lines.len() + new_lines.len());
        lines.extend(old_lines.iter().map(|l| format!("-{l}")));
        lines.extend(new_lines.iter().map(|l| format!("+{l}")));
        return lines;
    }
    lcs_diff_ops(&old_lines, &new_lines)
        .into_iter()
        .map(|op| match op {
            DiffOp::Equal(l) => format!(" {l}"),
            DiffOp::Delete(l) => format!("-{l}"),
            DiffOp::Insert(l) => format!("+{l}"),
        })
        .collect()
}

/// Format a summary like "Added 2 lines, removed 1 line" from exact counts.
pub fn diff_summary_from_counts(additions: usize, removals: usize) -> String {
    let mut parts = Vec::new();
    if additions > 0 {
        parts.push(format!(
            "Added {additions} {}",
            if additions == 1 { "line" } else { "lines" }
        ));
    }
    if removals > 0 {
        parts.push(format!(
            "{}emoved {removals} {}",
            if additions == 0 { "R" } else { "r" },
            if removals == 1 { "line" } else { "lines" }
        ));
    }
    if parts.is_empty() {
        "No changes".into()
    } else {
        parts.join(", ")
    }
}

/// Count additions and removals in `+`/`-` prefixed diff lines and
/// return a summary string like "Added 2 lines, removed 1 line".
pub fn diff_summary(diff_lines: &[String]) -> String {
    let additions = diff_lines.iter().filter(|l| l.starts_with('+')).count();
    let removals = diff_lines.iter().filter(|l| l.starts_with('-')).count();
    diff_summary_from_counts(additions, removals)
}

/// Number of context lines shown before and after the edit.
pub const EDIT_CONTEXT_LINES: usize = 3;

/// Build context diff lines (space/+/- prefixed) with surrounding
/// context from the original file. Returns `(lines, start_line)`
/// where `start_line` is the 1-based line number of the first line.
/// Falls back to plain +/- starting at line 1 if the original file
/// is unavailable.
pub fn build_context_diff(
    old_text: Option<&str>,
    new_text: &str,
    original_file: Option<&str>,
) -> (Vec<String>, usize) {
    let Some(original) = original_file else {
        return (generate_diff_lines(old_text, new_text), 1);
    };
    let old_fragment = old_text.unwrap_or("");

    // Find where old_fragment starts in the original file.
    let byte_offset = match if old_fragment.is_empty() {
        // For insertions (empty old_string), find position in new file
        // instead — but we don't have the new file byte offset here,
        // so fall back to plain diff.
        None
    } else {
        original.find(old_fragment)
    } {
        Some(offset) => offset,
        None => return (generate_diff_lines(old_text, new_text), 1),
    };

    let original_lines: Vec<&str> = original.lines().collect();
    let mut edit_start_line = original[..byte_offset].matches('\n').count();
    let mut old_line_count = old_fragment.lines().count();
    let mut changed_old = Cow::Borrowed(old_fragment);
    let mut changed_new = Cow::Borrowed(new_text);

    if !old_fragment.contains('\n') && !new_text.contains('\n') {
        let fragment_end = byte_offset + old_fragment.len();
        let line_start = original[..byte_offset]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let mut line_end = original[fragment_end..]
            .find('\n')
            .map_or(original.len(), |index| fragment_end + index);
        if line_end > line_start && original.as_bytes()[line_end - 1] == b'\r' {
            line_end -= 1;
        }

        if line_start < byte_offset || fragment_end < line_end {
            changed_old = Cow::Borrowed(&original[line_start..line_end]);
            changed_new = Cow::Owned(format!(
                "{}{}{}",
                &original[line_start..byte_offset],
                new_text,
                &original[fragment_end..line_end]
            ));
            edit_start_line = original[..line_start].matches('\n').count();
            old_line_count = 1;
        }
    }

    let context_start = edit_start_line.saturating_sub(EDIT_CONTEXT_LINES);
    let context_end_line = edit_start_line + old_line_count;
    let context_end = (context_end_line + EDIT_CONTEXT_LINES).min(original_lines.len());

    let mut lines = Vec::new();

    for i in context_start..edit_start_line {
        if let Some(line) = original_lines.get(i) {
            lines.push(format!(" {line}"));
        }
    }

    lines.extend(interleaved_diff_lines(
        changed_old.as_ref(),
        changed_new.as_ref(),
    ));

    // Context lines after the edit.
    for i in context_end_line..context_end {
        if let Some(line) = original_lines.get(i) {
            lines.push(format!(" {line}"));
        }
    }

    // 1-based start line.
    (lines, context_start + 1)
}

#[cfg(test)]
mod tests {
    use super::{
        build_context_diff, diff_line_counts, diff_summary, diff_summary_from_counts,
        generate_diff_lines, interleaved_diff_lines, lcs_diff_ops, DiffOp,
    };

    #[test]
    fn diff_summary_from_counts_preserves_summary_wording() {
        assert_eq!(
            diff_summary_from_counts(2, 1),
            "Added 2 lines, removed 1 line"
        );
        assert_eq!(diff_summary_from_counts(0, 2), "Removed 2 lines");
        assert_eq!(diff_summary_from_counts(0, 0), "No changes");
    }

    #[test]
    fn diff_line_counts_avoids_multi_edit_span_inflation() {
        let old = (0..1_269)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>();
        let mut new = old.clone();
        new[0] = "changed at start".into();
        new[600] = "changed in middle".into();
        new[1_268] = "changed at end".into();
        new.extend((0..31).map(|line| format!("inserted {line}")));

        assert_eq!(
            diff_line_counts(Some(&old.join("\n")), &new.join("\n")),
            (34, 3)
        );
    }

    #[test]
    fn diff_line_counts_handles_sparse_large_edits() {
        let old = (0..10_000)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>();
        let mut new = old.clone();
        new[100] = "first replacement".into();
        new[9_000] = "second replacement".into();

        assert_eq!(
            diff_line_counts(Some(&old.join("\n")), &new.join("\n")),
            (2, 2)
        );
    }

    #[test]
    fn diff_line_counts_handles_insert_delete_and_empty_text() {
        assert_eq!(diff_line_counts(Some("a\nb"), "a\nx\nb"), (1, 0));
        assert_eq!(diff_line_counts(Some("a\nx\nb"), "a\nb"), (0, 1));
        assert_eq!(diff_line_counts(Some("a\nb"), "a\nx"), (1, 1));
        assert_eq!(diff_line_counts(Some(""), ""), (0, 0));
        assert_eq!(diff_line_counts(Some(""), "a\nb"), (2, 0));
        assert_eq!(diff_line_counts(Some("a\nb"), ""), (0, 2));
        assert_eq!(diff_line_counts(None, ""), (1, 0));
    }

    #[test]
    fn diff_line_counts_bounds_unrelated_large_payloads() {
        let old = "old\n".repeat(10_000);
        let new = "new\n".repeat(12_000);
        assert_eq!(diff_line_counts(Some(&old), &new), (12_000, 10_000));
    }

    #[test]
    fn diff_line_counts_matches_lcs_for_small_repeated_sequences() {
        fn sequences(max_len: usize) -> Vec<Vec<&'static str>> {
            let mut values = vec![Vec::new()];
            for _ in 0..max_len {
                let next = values
                    .iter()
                    .filter(|value| value.len() < max_len)
                    .flat_map(|value| {
                        ["a", "b"].into_iter().map(move |line| {
                            let mut value = value.clone();
                            value.push(line);
                            value
                        })
                    })
                    .collect::<Vec<_>>();
                values.extend(next);
            }
            values.sort();
            values.dedup();
            values
        }

        let values = sequences(4);
        for old in &values {
            for new in &values {
                let expected = lcs_diff_ops(old, new).into_iter().fold(
                    (0, 0),
                    |(added, removed), op| match op {
                        DiffOp::Equal(_) => (added, removed),
                        DiffOp::Delete(_) => (added, removed + 1),
                        DiffOp::Insert(_) => (added + 1, removed),
                    },
                );
                assert_eq!(
                    diff_line_counts(Some(&old.join("\n")), &new.join("\n")),
                    expected,
                    "old={old:?}, new={new:?}"
                );
            }
        }
    }

    #[test]
    fn interleaved_diff_interleaves_changes_with_context() {
        // A real edit: keep lines A and C, replace B with X. The LCS pass
        // keeps the common lines as space-prefixed context and places the
        // removal next to its insertion (not all-removed-then-all-added).
        let lines = interleaved_diff_lines("line A\nline B\nline C", "line A\nline X\nline C");
        assert_eq!(
            lines,
            vec![
                " line A".to_string(),
                "-line B".to_string(),
                "+line X".to_string(),
                " line C".to_string(),
            ]
        );
    }

    #[test]
    fn interleaved_diff_degrades_when_no_common_lines() {
        // No shared lines → same shape as all-removed-then-all-added.
        let lines = interleaved_diff_lines("a\nb", "x\ny\nz");
        assert_eq!(
            lines,
            vec![
                "-a".to_string(),
                "-b".to_string(),
                "+x".to_string(),
                "+y".to_string(),
                "+z".to_string(),
            ]
        );
    }

    #[test]
    fn interleaved_diff_handles_pure_insert_and_delete() {
        assert_eq!(
            interleaved_diff_lines("", "n1\nn2"),
            vec!["+n1".to_string(), "+n2".to_string()]
        );
        assert_eq!(
            interleaved_diff_lines("o1\no2", ""),
            vec!["-o1".to_string(), "-o2".to_string()]
        );
    }

    #[test]
    fn interleaved_diff_head_shows_additions_not_only_removals() {
        // The whole point of the change: a head-truncated preview (take 4)
        // of an interleaved diff surfaces insertions, unlike the
        // all-removed-then-all-added shape whose head is only removals.
        let lines = interleaved_diff_lines("a\nL1\nb\nL2\nc\nL3", "a\nX1\nb\nX2\nc\nX3");
        let head: Vec<&String> = lines.iter().take(4).collect();
        assert!(
            head.iter().any(|l| l.starts_with('+')),
            "head should contain an addition: {head:?}"
        );
        assert!(
            head.iter().any(|l| l.starts_with('-')),
            "head should contain a removal: {head:?}"
        );
    }

    #[test]
    fn build_context_diff_interleaves_within_change_region() {
        // Original file contains the edited fragment; build_context_diff
        // wraps the interleaved change region with surrounding file context
        // and keeps the removal adjacent to its insertion.
        let original = "ctx1\nctx2\nctx3\nkeep\nold mid\ntail\nctx4\nctx5\nctx6";
        let (lines, _start) = build_context_diff(
            Some("keep\nold mid\ntail"),
            "keep\nnew mid\ntail",
            Some(original),
        );
        let minus = lines
            .iter()
            .position(|l| l == "-old mid")
            .unwrap_or_else(|| panic!("expected -old mid in {lines:?}"));
        assert_eq!(lines[minus + 1], "+new mid", "{lines:?}");
        // The shared fragment lines remain context, not churn.
        assert!(lines.contains(&" keep".to_string()), "{lines:?}");
        assert!(lines.contains(&" tail".to_string()), "{lines:?}");
    }

    #[test]
    fn build_context_diff_expands_inline_replacement_to_full_line() {
        let original = "pub const BUILTINS: &[(&str, &str)] = &[\n    (\"claude-code-guide\", \"文档问答（含 Web）\"),\n];";
        let (lines, start) = build_context_diff(
            Some("claude-code-guide"),
            "rebon-code-guide",
            Some(original),
        );

        assert_eq!(start, 1);
        assert_eq!(
            lines,
            vec![
                " pub const BUILTINS: &[(&str, &str)] = &[".to_string(),
                "-    (\"claude-code-guide\", \"文档问答（含 Web）\"),".to_string(),
                "+    (\"rebon-code-guide\", \"文档问答（含 Web）\"),".to_string(),
                " ];".to_string(),
            ]
        );
    }

    #[test]
    fn diff_summary_counts_remain_raw_totals() {
        // generate_diff_lines / diff_summary stay all-removed-then-added so
        // the summary keeps raw added/removed totals, independent of how
        // many lines the interleaved body shares as context.
        let simple = generate_diff_lines(Some("a\nb\nc"), "a\nX\nc");
        assert_eq!(diff_summary(&simple), "Added 3 lines, removed 3 lines");
    }
}
