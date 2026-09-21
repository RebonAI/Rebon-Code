//! One unified-diff hunk, reduced to the five fields the diff renderer
//! actually reads: the start line and line count on each side of the patch,
//! plus the raw `+`/`-`/space-prefixed lines.
//!
//! A full unified-diff hunk carries more than that — old and new file names,
//! function context headers, and so on — but nothing here reads them.
//! Modelling only the fields that are read keeps `PatchHunk` constructible
//! from any unified-diff parser at the seam, without committing to a
//! particular Rust diff crate.
//!
//! See the crate-level "Structured diffs: the injected seams and why" section
//! for the full rationale.

/// One unified-diff hunk, reduced to the fields the diff renderer reads.
///
/// `lines` is a vector of strings each prefixed with `+`, `-`, or a
/// space (the standard unified-diff line shape). The fallback module
/// parses these prefixes into `LineType::{Add, Remove, Nochange}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunk {
    /// First old-file line number this hunk covers (1-based).
    pub old_start: usize,
    /// Number of old-file lines this hunk covers.
    pub old_lines: usize,
    /// First new-file line number this hunk covers (1-based).
    pub new_start: usize,
    /// Number of new-file lines this hunk covers.
    pub new_lines: usize,
    /// Raw `+/-/space`-prefixed lines, in source order.
    pub lines: Vec<String>,
}

impl PatchHunk {
    /// Convenience constructor for tests and downstream callers.
    pub fn new(
        old_start: usize,
        old_lines: usize,
        new_start: usize,
        new_lines: usize,
        lines: Vec<String>,
    ) -> Self {
        Self {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_round_trips_all_fields() {
        let hunk = PatchHunk::new(
            10,
            3,
            10,
            4,
            vec![
                " unchanged".to_string(),
                "-removed".to_string(),
                "+added".to_string(),
                "+also added".to_string(),
                " trailing".to_string(),
            ],
        );
        assert_eq!(hunk.old_start, 10);
        assert_eq!(hunk.old_lines, 3);
        assert_eq!(hunk.new_start, 10);
        assert_eq!(hunk.new_lines, 4);
        assert_eq!(hunk.lines.len(), 5);
        assert_eq!(hunk.lines[0], " unchanged");
        assert_eq!(hunk.lines[2], "+added");
    }

    #[test]
    fn empty_lines_vector_is_legal() {
        // Some diff parsers emit zero-line hunks for pure deletions
        // at the end of a file. The slice must accept them.
        let hunk = PatchHunk::new(1, 0, 1, 0, vec![]);
        assert!(hunk.lines.is_empty());
    }

    #[test]
    fn clone_and_equality() {
        let hunk = PatchHunk::new(1, 1, 1, 1, vec![" a".into()]);
        let cloned = hunk.clone();
        assert_eq!(hunk, cloned);
    }
}
