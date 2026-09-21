//! Debounced ripgrep search across the workspace.
//!
//! ## Behaviour
//!
//! * The `VISIBLE_RESULTS = 12`, `DEBOUNCE_MS = 100`,
//!   `PREVIEW_CONTEXT_LINES = 4`, `MAX_MATCHES_PER_FILE = 10`,
//!   `MAX_TOTAL_MATCHES = 500` constants.
//! * The `preview_on_right` threshold (`columns >= 140`).
//! * The match-key projection (`{file}:{line}`).
//! * The `parse_ripgrep_line` scanner (Windows-aware, no regex dep).
//! * The match-label format (`5+ matches…`).
//! * The mention vs insert action discriminator
//!   (`@{file}#L{line}` vs `{file}:{line}`).
//! * The empty-state branch table (Searching… / No matches / Type to search…).
//! * The relative-path normalization (`starts_with('..')` falls back
//!   to absolute).
//!
//! ## Outbound seam
//!
//! Running ripgrep is the consumer's job. This module exposes a
//! reducer + parser only.

/// Number of result rows kept visible at once.
pub const VISIBLE_RESULTS: usize = 12;

/// Debounce delay before a query runs, in milliseconds.
pub const DEBOUNCE_MS: u64 = 100;

/// Context lines shown around a match in the preview.
pub const PREVIEW_CONTEXT_LINES: usize = 4;

/// Maximum matches kept per file.
pub const MAX_MATCHES_PER_FILE: usize = 10;

/// Maximum matches kept across all files.
pub const MAX_TOTAL_MATCHES: usize = 500;

/// The dialog title.
pub const TITLE: &str = "Global Search";

/// The query input placeholder.
pub const PLACEHOLDER: &str = "Type to search…";

/// Label for the select action.
pub const SELECT_ACTION: &str = "open in editor";

/// A single search match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalSearchMatch {
    /// File path (relative or absolute).
    pub file: String,
    /// 1-based line count.
    pub line: u64,
    /// The matched text.
    pub text: String,
}

/// Build the dedup key.
pub fn match_key(m: &GlobalSearchMatch) -> String {
    format!("{}:{}", m.file, m.line)
}

/// Parse one line of `rg -n --no-heading` output.
///
/// Format: `{file}:{line}:{text}`. The file path may contain colons, and a
/// Windows drive letter also gives it digits, so the boundary is found by
/// scanning left to right for the first `:` followed by digits and a `:` —
/// which is where `{line}` starts.
pub fn parse_ripgrep_line(line: &str) -> Option<GlobalSearchMatch> {
    // Lazy hand-rolled split — no regex crate dependency. We find
    // the first `:digit+:` from the left, equivalent to the lazy
    // regex `^(.*?):(\d+):(.*)$`.
    let bytes = line.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        // Find next ':'.
        let Some(colon) = line[idx..].find(':') else {
            return None;
        };
        let abs_colon = idx + colon;
        // Try to read digits after the colon.
        let after = abs_colon + 1;
        let mut end = after;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > after && end < bytes.len() && bytes[end] == b':' {
            let file = &line[..abs_colon];
            if file.is_empty() {
                return None;
            }
            let line_str = &line[after..end];
            let lineno: u64 = line_str.parse().ok()?;
            let text = &line[end + 1..];
            return Some(GlobalSearchMatch {
                file: file.to_string(),
                line: lineno,
                text: text.to_string(),
            });
        }
        idx = abs_colon + 1;
    }
    None
}

/// Compute the visible-rows count given the terminal height.
pub fn compute_visible_rows(rows: u16) -> usize {
    let r = rows as usize;
    let lower = 4_usize;
    let upper = VISIBLE_RESULTS;
    let candidate = r.saturating_sub(14).max(lower);
    candidate.min(upper)
}

/// True when the preview should be shown on the right of the list.
pub fn preview_on_right(columns: u16) -> bool {
    columns >= 140
}

/// Compute `(list_width, max_path_width, max_text_width, preview_width)`.
pub fn compute_layout(columns: u16) -> (usize, usize, usize, usize) {
    let cols = columns as usize;
    let on_right = preview_on_right(columns);
    let list_width = if on_right {
        cols.saturating_sub(10) / 2
    } else {
        cols.saturating_sub(8)
    };
    let max_path_width = (list_width * 40 / 100).max(20);
    let max_text_width = list_width
        .saturating_sub(max_path_width)
        .saturating_sub(4)
        .max(20);
    let preview_width = if on_right {
        cols.saturating_sub(list_width).saturating_sub(14).max(40)
    } else {
        cols.saturating_sub(6)
    };
    (list_width, max_path_width, max_text_width, preview_width)
}

/// Build the matches-summary label.
pub fn match_label(count: usize, truncated: bool, is_searching: bool) -> String {
    if count == 0 {
        return " ".to_string();
    }
    let plus = if truncated { "+" } else { "" };
    let suffix = if is_searching { "…" } else { "" };
    format!("{count}{plus} matches{suffix}")
}

/// Build the empty-state message.
pub fn empty_message(query: &str, is_searching: bool) -> &'static str {
    if is_searching {
        "Searching…"
    } else if query.trim().is_empty() {
        "Type to search…"
    } else {
        "No matches"
    }
}

/// Action emitted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalSearchAction {
    /// Open the matched file at the given line in the external editor.
    OpenInEditor {
        /// The matched file (relative path).
        file: String,
        /// The 1-based line count.
        line: u64,
    },
    /// Insert the path text into the input as a `@`-mention.
    InsertMention {
        /// `"@{file}#L{line} "`.
        text: String,
    },
    /// Insert the plain path text into the input.
    InsertPath {
        /// `"{file}:{line} "`.
        text: String,
    },
    /// Cancel the dialog.
    Cancel,
}

/// Reducer for the on-select event.
pub fn handle_select(m: &GlobalSearchMatch) -> GlobalSearchAction {
    GlobalSearchAction::OpenInEditor {
        file: m.file.clone(),
        line: m.line,
    }
}

/// Reducer for the on-insert event.
pub fn handle_insert(m: &GlobalSearchMatch, mention: bool) -> GlobalSearchAction {
    if mention {
        GlobalSearchAction::InsertMention {
            text: format!("@{}#L{} ", m.file, m.line),
        }
    } else {
        GlobalSearchAction::InsertPath {
            text: format!("{}:{} ", m.file, m.line),
        }
    }
}

/// Cancel handler.
pub fn handle_cancel() -> GlobalSearchAction {
    GlobalSearchAction::Cancel
}

/// Compute the relative path branch. The consumer computes
/// the match's path relative to the cwd; if the result starts with `..`, fall
/// back to the absolute path. We expose the helper so the consumer
/// can pass in the projected relative path.
pub fn project_match_file(absolute: &str, relative: &str) -> String {
    if relative.starts_with("..") {
        absolute.to_string()
    } else {
        relative.to_string()
    }
}

/// Append a fresh batch of matches to the existing list, deduping by
/// [`match_key`] and clamping to [`MAX_TOTAL_MATCHES`].
pub fn merge_matches(
    existing: &[GlobalSearchMatch],
    fresh: &[GlobalSearchMatch],
) -> Vec<GlobalSearchMatch> {
    let seen: std::collections::HashSet<String> = existing.iter().map(match_key).collect();
    let mut next: Vec<GlobalSearchMatch> = existing.to_vec();
    for m in fresh {
        if seen.contains(&match_key(m)) {
            continue;
        }
        next.push(m.clone());
        if next.len() >= MAX_TOTAL_MATCHES {
            break;
        }
    }
    if next.len() > MAX_TOTAL_MATCHES {
        next.truncate(MAX_TOTAL_MATCHES);
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(file: &str, line: u64, text: &str) -> GlobalSearchMatch {
        GlobalSearchMatch {
            file: file.into(),
            line,
            text: text.into(),
        }
    }

    #[test]
    fn constants_pinned() {
        assert_eq!(VISIBLE_RESULTS, 12);
        assert_eq!(DEBOUNCE_MS, 100);
        assert_eq!(PREVIEW_CONTEXT_LINES, 4);
        assert_eq!(MAX_MATCHES_PER_FILE, 10);
        assert_eq!(MAX_TOTAL_MATCHES, 500);
        assert_eq!(TITLE, "Global Search");
    }

    #[test]
    fn match_key_format() {
        assert_eq!(
            match_key(&m("src/foo.rs", 42, "let x = 1;")),
            "src/foo.rs:42"
        );
    }

    #[test]
    fn parse_ripgrep_simple() {
        let parsed = parse_ripgrep_line("src/foo.rs:42:let x = 1;").unwrap();
        assert_eq!(parsed.file, "src/foo.rs");
        assert_eq!(parsed.line, 42);
        assert_eq!(parsed.text, "let x = 1;");
    }

    #[test]
    fn parse_ripgrep_handles_colons_in_text() {
        let parsed = parse_ripgrep_line("src/x.rs:7:let url = \"http://x:8080\";").unwrap();
        assert_eq!(parsed.file, "src/x.rs");
        assert_eq!(parsed.line, 7);
        assert!(parsed.text.contains("http://x:8080"));
    }

    #[test]
    fn parse_ripgrep_handles_windows_drive_letter() {
        let parsed = parse_ripgrep_line("C:\\src\\foo.rs:10:hello").unwrap();
        assert_eq!(parsed.file, "C:\\src\\foo.rs");
        assert_eq!(parsed.line, 10);
        assert_eq!(parsed.text, "hello");
    }

    #[test]
    fn parse_ripgrep_rejects_no_colon() {
        assert!(parse_ripgrep_line("just text").is_none());
    }

    #[test]
    fn parse_ripgrep_rejects_no_line_number() {
        assert!(parse_ripgrep_line("foo:bar:baz").is_none());
    }

    #[test]
    fn parse_ripgrep_rejects_empty_file() {
        assert!(parse_ripgrep_line(":1:hello").is_none());
    }

    #[test]
    fn compute_visible_rows_table() {
        assert_eq!(compute_visible_rows(0), 4);
        assert_eq!(compute_visible_rows(15), 4);
        // 25 → 25-14 = 11 → max(4, 11) = 11 → min(12, 11) = 11
        assert_eq!(compute_visible_rows(25), 11);
        assert_eq!(compute_visible_rows(50), 12);
    }

    #[test]
    fn preview_on_right_threshold() {
        assert!(!preview_on_right(139));
        assert!(preview_on_right(140));
        assert!(preview_on_right(200));
    }

    #[test]
    fn compute_layout_narrow() {
        let (list, path, text, prev) = compute_layout(80);
        assert_eq!(list, 72);
        assert_eq!(path, 28);
        // 72 - 28 - 4 = 40
        assert_eq!(text, 40);
        // 80 - 6 = 74
        assert_eq!(prev, 74);
    }

    #[test]
    fn compute_layout_wide() {
        let (list, path, text, prev) = compute_layout(160);
        // (160 - 10) / 2 = 75
        assert_eq!(list, 75);
        // 75 * 40 / 100 = 30
        assert_eq!(path, 30);
        // 75 - 30 - 4 = 41
        assert_eq!(text, 41);
        // 160 - 75 - 14 = 71
        assert_eq!(prev, 71);
    }

    #[test]
    fn match_label_table() {
        assert_eq!(match_label(0, false, false), " ");
        assert_eq!(match_label(5, false, false), "5 matches");
        assert_eq!(match_label(5, false, true), "5 matches…");
        assert_eq!(match_label(500, true, false), "500+ matches");
        assert_eq!(match_label(500, true, true), "500+ matches…");
    }

    #[test]
    fn empty_message_table() {
        assert_eq!(empty_message("", false), "Type to search…");
        assert_eq!(empty_message("foo", false), "No matches");
        assert_eq!(empty_message("", true), "Searching…");
        assert_eq!(empty_message("foo", true), "Searching…");
    }

    #[test]
    fn handle_select_emits_open() {
        let action = handle_select(&m("src/x.rs", 5, "x"));
        assert_eq!(
            action,
            GlobalSearchAction::OpenInEditor {
                file: "src/x.rs".into(),
                line: 5,
            }
        );
    }

    #[test]
    fn handle_insert_mention_format() {
        let action = handle_insert(&m("src/x.rs", 5, "x"), true);
        assert_eq!(
            action,
            GlobalSearchAction::InsertMention {
                text: "@src/x.rs#L5 ".into()
            }
        );
    }

    #[test]
    fn handle_insert_no_mention_format() {
        let action = handle_insert(&m("src/x.rs", 5, "x"), false);
        assert_eq!(
            action,
            GlobalSearchAction::InsertPath {
                text: "src/x.rs:5 ".into()
            }
        );
    }

    #[test]
    fn handle_cancel_emits_cancel() {
        assert_eq!(handle_cancel(), GlobalSearchAction::Cancel);
    }

    #[test]
    fn project_match_file_relative() {
        assert_eq!(project_match_file("/abs/p", "rel/p"), "rel/p");
    }

    #[test]
    fn project_match_file_falls_back_to_abs() {
        assert_eq!(project_match_file("/abs/p", "../sibling"), "/abs/p");
    }

    #[test]
    fn merge_matches_dedupes() {
        let existing = vec![m("a", 1, "x")];
        let fresh = vec![m("a", 1, "x"), m("b", 2, "y")];
        let merged = merge_matches(&existing, &fresh);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], existing[0]);
        assert_eq!(merged[1].file, "b");
    }

    #[test]
    fn merge_matches_clamps_to_max() {
        let existing: Vec<GlobalSearchMatch> = (0..MAX_TOTAL_MATCHES - 1)
            .map(|i| m(&format!("f{i}"), 1, ""))
            .collect();
        let fresh = vec![m("new1", 1, ""), m("new2", 1, ""), m("new3", 1, "")];
        let merged = merge_matches(&existing, &fresh);
        assert_eq!(merged.len(), MAX_TOTAL_MATCHES);
    }

    #[test]
    fn merge_matches_preserves_existing_when_fresh_dups() {
        let existing = vec![m("a", 1, "x")];
        let fresh = vec![m("a", 1, "x")];
        let merged = merge_matches(&existing, &fresh);
        assert_eq!(merged.len(), 1);
    }
}
