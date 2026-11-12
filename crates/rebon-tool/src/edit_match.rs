//! Matching `old_string` against a file's current content, and explaining a
//! miss well enough that the model can retry without another `Read`.
//!
//! `Edit` and `MultiEdit` match against whatever is on disk when they run —
//! there is no read-state precondition, so a formatter or another agent
//! touching the file is not by itself a failure. What can still fail is the
//! anchor: it is gone, or it is not unique. Both errors carry the current
//! text, rendered the way `Read` renders it (`{:>6}\t{line}`):
//!
//! * **Not found** — the region that most resembles `old_string`, with a few
//!   lines either side, found by scoring each `old_string`-sized window by
//!   line similarity (whitespace ignored). The scan is bounded: small files
//!   are scanned window by window, large ones through the few most
//!   distinctive `old_string` lines.
//! * **Ambiguous** — every match's line number (up to a cap) with the lines
//!   around it, so the model can add the context that picks one.

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::edit::normalize_line_endings;

/// Lines shown before and after the closest region.
const CONTEXT_LINES: usize = 3;
/// Most lines one closest-region snippet shows.
const MAX_SNIPPET_LINES: usize = 30;
/// Longest rendered line, in characters; longer lines are clipped.
const MAX_RENDERED_LINE_CHARS: usize = 160;
/// Matches listed in full for an ambiguous `old_string`.
const MAX_LISTED_MATCHES: usize = 10;
/// Lines shown from the top when nothing resembles `old_string`.
const HEAD_LINES: usize = 10;
/// Files up to this many lines are scanned window by window...
const FULL_SCAN_MAX_LINES: usize = 5_000;
/// ...as long as the scan stays within this many line comparisons.
const FULL_SCAN_BUDGET: usize = 100_000;
/// Larger files: lines searched for the distinctive `old_string` lines.
const MAX_SCANNED_LINES: usize = 200_000;
/// Larger files: how many `old_string` lines are searched for.
const ANCHOR_LINES: usize = 3;
/// Larger files: how many candidate windows are scored in full.
const ANCHOR_CANDIDATES: usize = 8;
/// Characters of a line that take part in similarity scoring.
const MAX_SCORED_CHARS: usize = 160;
/// Below this average line similarity a region is not offered as the
/// closest match.
const SIMILARITY_FLOOR: f64 = 0.5;

/// How `old_string` matched the current content: the spelling that matched
/// (line endings adapted to the file) and the replacement spelled the same
/// way.
pub(crate) struct Anchor<'a> {
    pub(crate) old: Cow<'a, str>,
    pub(crate) new: Cow<'a, str>,
    pub(crate) occurrences: usize,
}

/// Count `old_string` in `content`, adapting line endings to the file.
///
/// Models send bare `\n`. A CRLF file gets `old_string` / `new_string`
/// converted to CRLF; when that finds nothing in a file that mixes both
/// styles, the bare-LF spelling is tried too, so a region that happens to
/// use LF still matches exactly.
pub(crate) fn find_anchor<'a>(
    content: &str,
    old_string: &'a str,
    new_string: &'a str,
) -> Anchor<'a> {
    let file_uses_crlf = content.contains("\r\n");
    let old = normalize_line_endings(old_string, file_uses_crlf);
    let occurrences = content.matches(&*old).count();
    if occurrences > 0 || !file_uses_crlf || !old_string.contains('\n') {
        return Anchor {
            old,
            new: normalize_line_endings(new_string, file_uses_crlf),
            occurrences,
        };
    }
    let lf_old = normalize_line_endings(old_string, false);
    let lf_occurrences = content.matches(&*lf_old).count();
    if lf_occurrences > 0 {
        return Anchor {
            old: lf_old,
            new: normalize_line_endings(new_string, false),
            occurrences: lf_occurrences,
        };
    }
    Anchor {
        old,
        new: normalize_line_endings(new_string, true),
        occurrences: 0,
    }
}

/// Explain an `old_string` that does not occur in `content`: the closest
/// current region with line numbers, or — when nothing resembles it — the
/// top of the file. The caller supplies the leading "was not found" sentence.
pub(crate) fn closest_region_report(content: &str, old_string: &str) -> String {
    if content.is_empty() {
        return "The file is empty.".to_string();
    }
    let file_lines: Vec<&str> = content.lines().collect();
    let (old_lines, had_line_number_prefixes) = anchor_lines(old_string);
    let mut out = String::new();
    if had_line_number_prefixes {
        out.push_str(
            "`old_string` appears to include the line-number prefixes of Read output; \
             they are not part of the file. ",
        );
    }

    let search = if old_lines.is_empty() {
        None
    } else {
        closest_window(&file_lines, &old_lines)
    };
    match search {
        Some(found) if found.score >= SIMILARITY_FLOOR => {
            let first = found.start + 1;
            let last = found.start + found.len;
            let lines = if found.len <= 1 {
                format!("line {first}")
            } else {
                format!("lines {first}-{last}")
            };
            let likeness = if found.whitespace_only {
                "identical apart from whitespace, indentation, or line endings".to_string()
            } else {
                // Never round a real difference up to "100%".
                let percent = ((found.score * 100.0).floor() as u32).min(99);
                format!("{percent}% similar")
            };
            let _ = writeln!(
                out,
                "The closest match is {lines} ({likeness}). Current content there:"
            );
            out.push_str(&render_region(&file_lines, found.start, found.len));
            out.push_str(
                "\nBuild `old_string` from the current text above (everything after the \
                 line-number tab) and retry.",
            );
        }
        _ => {
            out.push_str("No part of the file closely resembles `old_string`");
            if search.is_some_and(|found| found.truncated_search) {
                let _ = write!(
                    out,
                    " (only the first {MAX_SCANNED_LINES} lines were searched)"
                );
            }
            let shown = file_lines.len().min(HEAD_LINES);
            let _ = writeln!(
                out,
                ". The file has {} line{}; the first {shown}:",
                file_lines.len(),
                if file_lines.len() == 1 { "" } else { "s" }
            );
            out.push_str(&render_lines(&file_lines, 0, shown));
        }
    }
    out
}

/// List where an ambiguous `needle` matches in `content`: each match's
/// starting line with the line before and after, up to a cap, plus how many
/// were left out. The caller supplies the leading "matched N times" sentence
/// and the advice.
pub(crate) fn match_listing(content: &str, needle: &str, occurrences: usize) -> String {
    let file_lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    let mut line_index = 0usize;
    let mut counted_to = 0usize;
    for (offset, _) in content.match_indices(needle).take(MAX_LISTED_MATCHES) {
        line_index += content.as_bytes()[counted_to..offset]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        counted_to = offset;
        let from = line_index.saturating_sub(1);
        let to = (line_index + 2).min(file_lines.len()).max(from);
        let _ = writeln!(out, "line {}:", line_index + 1);
        out.push_str(&render_lines(&file_lines, from, to));
        out.push('\n');
    }
    if occurrences > MAX_LISTED_MATCHES {
        let _ = writeln!(
            out,
            "({} more match{} not shown)",
            occurrences - MAX_LISTED_MATCHES,
            if occurrences - MAX_LISTED_MATCHES == 1 {
                ""
            } else {
                "es"
            }
        );
    }
    out
}

/// One `old_string` line, prepared for scoring.
struct ScoredLine {
    /// The line with all whitespace removed, capped at `MAX_SCORED_CHARS`.
    key: String,
    /// Sorted character bigrams of `key`.
    grams: Vec<u64>,
}

impl ScoredLine {
    fn new(line: &str) -> Self {
        let key: String = line
            .chars()
            .filter(|c| !c.is_whitespace())
            .take(MAX_SCORED_CHARS)
            .collect();
        let chars: Vec<char> = key.chars().collect();
        let mut grams: Vec<u64> = chars
            .windows(2)
            .map(|pair| ((pair[0] as u64) << 32) | pair[1] as u64)
            .collect();
        grams.sort_unstable();
        Self { key, grams }
    }
}

/// Dice coefficient over character bigrams, whitespace ignored: 1.0 for
/// lines that differ only in whitespace, 0.0 for nothing in common.
fn similarity(a: &ScoredLine, b: &ScoredLine) -> f64 {
    if a.key == b.key {
        return 1.0;
    }
    if a.grams.is_empty() || b.grams.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.grams.len() && j < b.grams.len() {
        match a.grams[i].cmp(&b.grams[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    (2 * common) as f64 / (a.grams.len() + b.grams.len()) as f64
}

/// The lines of `old_string` the locator searches for: leading and trailing
/// blank lines dropped, and `Read`'s line-number prefixes stripped when every
/// non-blank line carries one. The flag reports that stripping.
fn anchor_lines(old_string: &str) -> (Vec<String>, bool) {
    let raw: Vec<&str> = old_string.lines().collect();
    let non_blank: Vec<&&str> = raw.iter().filter(|line| !line.trim().is_empty()).collect();
    let prefixed = !non_blank.is_empty()
        && non_blank
            .iter()
            .all(|line| strip_line_number_prefix(line).is_some());
    let mut lines: Vec<String> = raw
        .iter()
        .map(|line| {
            if prefixed {
                strip_line_number_prefix(line).unwrap_or(line).to_string()
            } else {
                (*line).to_string()
            }
        })
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let leading_blank = lines
        .iter()
        .take_while(|line| line.trim().is_empty())
        .count();
    lines.drain(..leading_blank);
    (lines, prefixed)
}

/// `   12\tbody` or `12→body` → `body`.
fn strip_line_number_prefix(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(' ');
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &trimmed[digits..];
    rest.strip_prefix('\t').or_else(|| rest.strip_prefix('→'))
}

#[derive(Clone, Copy)]
struct Window {
    /// 0-based first line.
    start: usize,
    /// Lines in the window (shorter than `old_string` at the end of the file).
    len: usize,
    /// Average line similarity over `old_string`'s lines.
    score: f64,
    /// Every line matches `old_string` once whitespace is ignored.
    whitespace_only: bool,
    /// The large-file search stopped before the end of the file.
    truncated_search: bool,
}

fn closest_window(file_lines: &[&str], old_lines: &[String]) -> Option<Window> {
    let old: Vec<ScoredLine> = old_lines.iter().map(|line| ScoredLine::new(line)).collect();
    let m = file_lines.len();
    let n = old.len();
    if m == 0 || n == 0 {
        return None;
    }

    let mut best: Option<(usize, f64)> = None;
    let mut consider = |start: usize, score: f64| {
        if best.is_none_or(|(_, top)| score > top) {
            best = Some((start, score));
        }
    };
    let mut truncated_search = false;

    if m <= FULL_SCAN_MAX_LINES && m.saturating_mul(n) <= FULL_SCAN_BUDGET {
        let file: Vec<ScoredLine> = file_lines
            .iter()
            .map(|line| ScoredLine::new(line))
            .collect();
        for start in 0..=m.saturating_sub(n) {
            let len = n.min(m - start);
            let total: f64 = (0..len)
                .map(|offset| similarity(&file[start + offset], &old[offset]))
                .sum();
            consider(start, total / n as f64);
        }
    } else {
        // Find the few most distinctive `old_string` lines, then score only
        // the windows those hits imply. Each anchor keeps its own best hits,
        // so a common line (a lone `}`) cannot crowd out a rare one.
        let mut anchors: Vec<usize> = (0..n).collect();
        anchors.sort_by_key(|&index| std::cmp::Reverse(old[index].key.chars().count()));
        anchors.truncate(ANCHOR_LINES);

        let scanned = m.min(MAX_SCANNED_LINES);
        truncated_search = scanned < m;
        let mut hits: Vec<Vec<(f64, usize)>> = vec![Vec::new(); anchors.len()];
        for (index, line) in file_lines.iter().take(scanned).enumerate() {
            let scored = ScoredLine::new(line);
            for (slot, &anchor) in anchors.iter().enumerate() {
                let sim = similarity(&scored, &old[anchor]);
                let list = &mut hits[slot];
                if sim < SIMILARITY_FLOOR
                    || (list.len() == ANCHOR_CANDIDATES
                        && list.last().is_some_and(|(lowest, _)| sim <= *lowest))
                {
                    continue;
                }
                let at = list
                    .iter()
                    .position(|(score, _)| sim > *score)
                    .unwrap_or(list.len());
                list.insert(at, (sim, index.saturating_sub(anchor)));
                list.truncate(ANCHOR_CANDIDATES);
            }
        }
        let mut starts: Vec<usize> = hits.into_iter().flatten().map(|(_, start)| start).collect();
        starts.sort_unstable();
        starts.dedup();
        for start in starts {
            let len = n.min(m - start);
            let total: f64 = (0..len)
                .map(|offset| {
                    similarity(&ScoredLine::new(file_lines[start + offset]), &old[offset])
                })
                .sum();
            consider(start, total / n as f64);
        }
    }

    let Some((start, score)) = best else {
        return Some(Window {
            start: 0,
            len: 0,
            score: 0.0,
            whitespace_only: false,
            truncated_search,
        });
    };
    let len = n.min(m - start);
    let whitespace_only = len == n
        && (0..len).all(|offset| {
            without_whitespace(file_lines[start + offset]) == without_whitespace(&old_lines[offset])
        });
    Some(Window {
        start,
        len,
        score,
        whitespace_only,
        truncated_search,
    })
}

fn without_whitespace(line: &str) -> String {
    line.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The region `[start, start + len)` with `CONTEXT_LINES` either side,
/// capped at `MAX_SNIPPET_LINES`.
fn render_region(file_lines: &[&str], start: usize, len: usize) -> String {
    let from = start.saturating_sub(CONTEXT_LINES);
    let wanted_to = (start + len + CONTEXT_LINES).min(file_lines.len());
    let to = wanted_to.min(from + MAX_SNIPPET_LINES);
    let mut out = render_lines(file_lines, from, to);
    if to < wanted_to {
        let _ = write!(out, "\n   ... ({} more lines not shown)", wanted_to - to);
    }
    out
}

/// Lines `[from, to)` rendered the way `Read` renders them.
fn render_lines(file_lines: &[&str], from: usize, to: usize) -> String {
    let mut out = String::new();
    for (index, line) in file_lines.iter().enumerate().take(to).skip(from) {
        if index > from {
            out.push('\n');
        }
        let _ = write!(out, "{:>6}\t{}", index + 1, clip(line));
    }
    out
}

fn clip(line: &str) -> Cow<'_, str> {
    match line.char_indices().nth(MAX_RENDERED_LINE_CHARS) {
        Some((cut, _)) => Cow::Owned(format!("{}…", &line[..cut])),
        None => Cow::Borrowed(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_file_matches_lf_old_string() {
        let anchor = find_anchor("a\r\nb\r\n", "a\nb", "x\ny");
        assert_eq!(anchor.occurrences, 1);
        assert_eq!(anchor.old, "a\r\nb");
        assert_eq!(anchor.new, "x\r\ny");
    }

    #[test]
    fn mixed_line_endings_fall_back_to_the_lf_spelling() {
        let anchor = find_anchor("head\r\nfoo\nbar\ntail\r\n", "foo\nbar", "FOO\nBAR");
        assert_eq!(anchor.occurrences, 1);
        assert_eq!(anchor.old, "foo\nbar");
        assert_eq!(anchor.new, "FOO\nBAR");
    }

    #[test]
    fn indentation_miss_points_at_the_region_and_says_why() {
        let content = "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"{a}{b}\");\n}\n";
        let report = closest_region_report(content, "let a = 1;\nlet b = 2;");
        assert!(report.contains("lines 2-3"), "{report}");
        assert!(report.contains("whitespace"), "{report}");
        assert!(report.contains("     2\t    let a = 1;"), "{report}");
        assert!(report.contains("     1\tfn main() {"), "{report}");
        assert!(report.contains("     5\t}"), "{report}");
    }

    #[test]
    fn a_changed_line_is_found_by_similarity() {
        let mut content = String::new();
        for i in 1..=40 {
            let _ = writeln!(content, "let value_{i} = compute({i});");
        }
        let report = closest_region_report(
            &content,
            "let value_20 = compute(20);\nlet value_21 = compute_fast(21);",
        );
        assert!(report.contains("lines 20-21"), "{report}");
        assert!(report.contains("% similar"), "{report}");
        assert!(report.contains("    17\t"), "{report}");
        assert!(report.contains("    24\t"), "{report}");
    }

    #[test]
    fn large_files_use_the_distinctive_lines() {
        let mut content = String::new();
        for i in 1..=20_000 {
            let _ = writeln!(content, "line number {i}");
        }
        content.push_str("    fn the_unusual_function_name(argument: u32) -> u32 {\n");
        content.push_str("        argument + 1\n    }\n");
        let report = closest_region_report(
            &content,
            "fn the_unusual_function_name(argument: u64) -> u64 {\n    argument + 1\n}",
        );
        assert!(report.contains("lines 20001-20003"), "{report}");
    }

    #[test]
    fn nothing_similar_shows_the_top_of_the_file() {
        let report = closest_region_report("alpha\nbeta\n", "zzzzzzzzzzzz qqqqqqq");
        assert!(report.contains("No part of the file"), "{report}");
        assert!(report.contains("     1\talpha"), "{report}");
        assert!(report.contains("     2\tbeta"), "{report}");
    }

    #[test]
    fn empty_file_is_named() {
        assert_eq!(closest_region_report("", "x"), "The file is empty.");
    }

    #[test]
    fn read_line_number_prefixes_are_called_out() {
        let content = "one\ntwo\nthree\n";
        let report = closest_region_report(content, "     2\ttwo\n     3\tthree");
        assert!(report.contains("line-number prefixes"), "{report}");
        assert!(report.contains("lines 2-3"), "{report}");
    }

    #[test]
    fn match_listing_names_each_line_and_counts_the_rest() {
        let content = (1..=15)
            .map(|i| format!("row {i}\nneedle\n"))
            .collect::<String>();
        let listing = match_listing(&content, "needle", 15);
        assert!(listing.contains("line 2:"), "{listing}");
        assert!(listing.contains("     1\trow 1"), "{listing}");
        assert!(listing.contains("     3\trow 2"), "{listing}");
        assert!(listing.contains("line 20:"), "{listing}");
        assert!(!listing.contains("line 22:"), "{listing}");
        assert!(listing.contains("(5 more matches not shown)"), "{listing}");
    }

    #[test]
    fn reports_stay_bounded() {
        let long = "x".repeat(10_000);
        let content = format!("{long}\n").repeat(200);
        let report = closest_region_report(&content, &format!("{long}y\n").repeat(100));
        assert!(report.len() < 8 * 1024, "{}", report.len());
        let listing = match_listing(&content, &long, 200);
        assert!(listing.len() < 8 * 1024, "{}", listing.len());
    }
}
