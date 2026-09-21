//! File-edit message projections: an applied file edit, a rejected file
//! write or update, and a rejected notebook edit.

use std::path::Path;

use crate::{
    format_user_prompt_hidden_separator, USER_PROMPT_FOLD_HEAD_LINES, USER_PROMPT_FOLD_TAIL_LINES,
    USER_PROMPT_FOLD_THRESHOLD_LINES,
};
use crate::{LineColor, LineSegment, RenderedLine, WordColor};

/// Shared line cap.
pub const MAX_LINES_TO_RENDER: usize = 10;

/// One hunk of a structured patch; only its lines are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    /// Hunk lines with leading diff markers.
    pub lines: Vec<String>,
}

/// Input bag for [`project_file_edit_updated`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditUpdatedInput {
    /// Path of the edited file.
    pub file_path: String,
    /// The edit's patch hunks.
    pub structured_patch: Vec<StructuredPatchHunk>,
    /// First line of the file, when known.
    pub first_line: Option<String>,
    /// Full file content, when available.
    pub file_content: Option<String>,
    /// True in the condensed output style.
    pub style_condensed: bool,
    /// Global verbose flag.
    pub verbose: bool,
    /// Hint shown instead of the diff, when the edit supplies one.
    pub preview_hint: Option<String>,
    /// Terminal width in columns.
    pub columns: usize,
}

/// Output projection for [`project_file_edit_updated`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEditUpdatedProjection {
    /// Hint-only branch for non-condensed plan files.
    PreviewHint {
        /// The preview hint text.
        hint: String,
    },
    /// Summary-only condensed branch.
    SummaryOnly {
        /// Added/removed summary.
        summary: String,
    },
    /// Full diff branch.
    Detailed {
        /// Added/removed summary.
        summary: String,
        /// `columns - 12`
        diff_width: usize,
        /// Whether long diff runs use the prompt-style head/tail fold.
        fold_long_runs: bool,
        /// Path of the edited file.
        file_path: String,
        /// First line of the file, when known.
        first_line: Option<String>,
        /// Full file content, when available.
        file_content: Option<String>,
        /// The edit's patch hunks.
        structured_patch: Vec<StructuredPatchHunk>,
    },
}

/// Project an applied file edit: the preview hint alone, a condensed
/// summary, or the summary with the full diff.
pub fn project_file_edit_updated(input: &FileEditUpdatedInput) -> FileEditUpdatedProjection {
    let additions = input
        .structured_patch
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .filter(|line| line.starts_with('+'))
        .count();
    let removals = input
        .structured_patch
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .filter(|line| line.starts_with('-'))
        .count();
    let change_summary = summarize_additions_and_removals(additions, removals);
    // Include file path in the summary so the tool is identifiable
    // even in condensed mode (matching the streaming overlay which
    // renders "Edit (path)" as a header line).
    let summary = if change_summary.is_empty() {
        format!("Edit ({})", input.file_path)
    } else {
        format!("Edit ({}) · {}", input.file_path, change_summary)
    };

    if let Some(preview_hint) = input.preview_hint.as_ref() {
        if !input.style_condensed && !input.verbose {
            return FileEditUpdatedProjection::PreviewHint {
                hint: preview_hint.clone(),
            };
        }
    } else if input.style_condensed && !input.verbose {
        return FileEditUpdatedProjection::SummaryOnly { summary };
    }

    FileEditUpdatedProjection::Detailed {
        summary,
        diff_width: input.columns.saturating_sub(12),
        fold_long_runs: !input.verbose,
        file_path: input.file_path.clone(),
        first_line: input.first_line.clone(),
        file_content: input.file_content.clone(),
        structured_patch: input.structured_patch.clone(),
    }
}

/// `write` vs `update` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEditOperation {
    /// `write`
    Write,
    /// `update`
    Update,
}

/// Input bag for [`project_file_edit_rejected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditRejectedInput {
    /// `file_path`
    pub file_path: String,
    /// `operation`
    pub operation: FileEditOperation,
    /// `patch`
    pub patch: Vec<StructuredPatchHunk>,
    /// First line of the file, when known.
    pub first_line: Option<String>,
    /// Full file content, when available.
    pub file_content: Option<String>,
    /// `content`
    pub content: Option<String>,
    /// True in the condensed output style.
    pub style_condensed: bool,
    /// `verbose`
    pub verbose: bool,
    /// `columns`
    pub columns: usize,
    /// Current working directory the displayed path is made relative to.
    pub cwd: Option<String>,
}

/// Output projection for [`project_file_edit_rejected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEditRejectedProjection {
    /// Summary-only branch.
    SummaryOnly {
        /// Header text.
        summary: String,
    },
    /// New-file content preview branch.
    WritePreview {
        /// Header text.
        summary: String,
        /// Truncated preview text.
        preview: String,
        /// `columns - 12`
        preview_width: usize,
        /// Hidden lines beyond the 10-line cap.
        hidden_line_count: usize,
        /// Highlighted-code file path.
        file_path: String,
    },
    /// Diff preview branch for updates.
    DiffPreview {
        /// Header text.
        summary: String,
        /// `columns - 12`
        diff_width: usize,
        /// Whether long diff runs use the prompt-style head/tail fold.
        fold_long_runs: bool,
        /// `file_path`
        file_path: String,
        /// First line of the file, when known.
        first_line: Option<String>,
        /// Full file content, when available.
        file_content: Option<String>,
        /// `patch`
        patch: Vec<StructuredPatchHunk>,
    },
}

/// Project a rejected file write or update: a summary alone, a preview of
/// the content that would have been written, or a diff preview.
pub fn project_file_edit_rejected(input: &FileEditRejectedInput) -> FileEditRejectedProjection {
    let summary = format!(
        "User rejected {} to {}",
        match input.operation {
            FileEditOperation::Write => "write",
            FileEditOperation::Update => "update",
        },
        display_path(&input.file_path, input.cwd.as_deref(), input.verbose)
    );

    if input.style_condensed && !input.verbose {
        return FileEditRejectedProjection::SummaryOnly { summary };
    }

    if matches!(input.operation, FileEditOperation::Write) {
        if let Some(content) = input.content.as_ref() {
            let lines = content.lines().collect::<Vec<_>>();
            let hidden_line_count = if input.verbose {
                0
            } else {
                lines.len().saturating_sub(MAX_LINES_TO_RENDER)
            };
            let preview = if input.verbose {
                content.clone()
            } else {
                lines
                    .into_iter()
                    .take(MAX_LINES_TO_RENDER)
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            return FileEditRejectedProjection::WritePreview {
                summary,
                preview: if preview.is_empty() {
                    "(No content)".into()
                } else {
                    preview
                },
                preview_width: input.columns.saturating_sub(12),
                hidden_line_count,
                file_path: input.file_path.clone(),
            };
        }
    }

    if input.patch.is_empty() {
        return FileEditRejectedProjection::SummaryOnly { summary };
    }

    FileEditRejectedProjection::DiffPreview {
        summary,
        diff_width: input.columns.saturating_sub(12),
        fold_long_runs: !input.verbose,
        file_path: input.file_path.clone(),
        first_line: input.first_line.clone(),
        file_content: input.file_content.clone(),
        patch: input.patch.clone(),
    }
}

/// Notebook edit mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotebookEditMode {
    /// `replace`
    Replace,
    /// `insert`
    Insert,
    /// `delete`
    Delete,
}

/// Notebook cell type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotebookCellType {
    /// `code`
    Code,
    /// `markdown`
    Markdown,
}

/// Input bag for [`project_notebook_edit_rejected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookEditRejectedInput {
    /// `notebook_path`
    pub notebook_path: String,
    /// `cell_id`
    pub cell_id: Option<String>,
    /// `new_source`
    pub new_source: String,
    /// `cell_type`
    pub cell_type: Option<NotebookCellType>,
    /// `edit_mode`
    pub edit_mode: NotebookEditMode,
    /// `verbose`
    pub verbose: bool,
    /// Current working directory.
    pub cwd: Option<String>,
}

/// Output projection for [`project_notebook_edit_rejected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookEditRejectedProjection {
    /// Header text.
    pub summary: String,
    /// Preview snippet, hidden for delete mode.
    pub preview: Option<String>,
    /// Highlighted-code file path (`file.md` / `file.py`).
    pub preview_file_path: Option<String>,
}

/// Project a rejected notebook edit: a summary naming the cell, plus a
/// preview of the new source unless the edit was a delete.
pub fn project_notebook_edit_rejected(
    input: &NotebookEditRejectedInput,
) -> NotebookEditRejectedProjection {
    let operation = match input.edit_mode {
        NotebookEditMode::Delete => "delete".to_owned(),
        NotebookEditMode::Replace => "replace cell in".to_owned(),
        NotebookEditMode::Insert => "insert cell in".to_owned(),
    };
    let summary = format!(
        "User rejected {} {} at cell {}",
        operation,
        display_path(&input.notebook_path, input.cwd.as_deref(), input.verbose),
        input.cell_id.as_deref().unwrap_or("")
    );

    let (preview, preview_file_path) = if matches!(input.edit_mode, NotebookEditMode::Delete) {
        (None, None)
    } else {
        (
            Some(input.new_source.clone()),
            Some(match input.cell_type.unwrap_or(NotebookCellType::Code) {
                NotebookCellType::Markdown => "file.md".into(),
                NotebookCellType::Code => "file.py".into(),
            }),
        )
    };

    NotebookEditRejectedProjection {
        summary,
        preview,
        preview_file_path,
    }
}

/// Fold consecutive rendered diff rows of the same kind with the same head/tail
/// thresholds and separator used by long user prompts. This includes unchanged
/// rows between distant MultiEdit replacements, not only added or removed rows.
/// Formatting happens before folding, so retained tail rows keep their original
/// line numbers.
pub fn fold_long_diff_runs(lines: Vec<RenderedLine>, width: usize) -> Vec<RenderedLine> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RunKind {
        Added,
        Removed,
        Unchanged,
    }

    fn run_kind(line: &RenderedLine) -> RunKind {
        match line.line_color {
            LineColor::Added | LineColor::AddedDimmed => RunKind::Added,
            LineColor::Removed | LineColor::RemovedDimmed => RunKind::Removed,
            LineColor::None => RunKind::Unchanged,
        }
    }

    fn hidden_separator(
        hidden_line_count: usize,
        width: usize,
        gutter_width: usize,
        line_color: LineColor,
    ) -> RenderedLine {
        let gutter_width = gutter_width.min(width);
        let content_width = width.saturating_sub(gutter_width);
        // Leave one cell free: ratatui's wrapped Paragraph can emit an empty
        // continuation row when a styled line exactly fills the available width.
        let separator_width = content_width.saturating_sub(1);
        RenderedLine {
            gutter: " ".repeat(gutter_width),
            content: vec![LineSegment {
                text: format_user_prompt_hidden_separator(hidden_line_count, separator_width),
                word_color: WordColor::None,
            }],
            padding: String::new(),
            line_color,
            dim: true,
        }
    }

    let mut folded = Vec::with_capacity(lines.len());
    let mut lines = lines.into_iter().peekable();
    while let Some(line) = lines.next() {
        let kind = run_kind(&line);
        let mut run = vec![line];
        while lines.peek().is_some_and(|next| run_kind(next) == kind) {
            run.push(lines.next().expect("peeked diff line must exist"));
        }

        let hidden_line_count = run
            .len()
            .saturating_sub(USER_PROMPT_FOLD_HEAD_LINES + USER_PROMPT_FOLD_TAIL_LINES);
        if run.len() < USER_PROMPT_FOLD_THRESHOLD_LINES || hidden_line_count == 0 {
            folded.extend(run);
            continue;
        }

        let gutter_width = run
            .first()
            .map(|line| line.gutter.chars().count())
            .unwrap_or(0);
        let line_color = run[0].line_color;
        let tail = run.split_off(run.len() - USER_PROMPT_FOLD_TAIL_LINES);
        run.truncate(USER_PROMPT_FOLD_HEAD_LINES);
        folded.extend(run);
        folded.push(hidden_separator(
            hidden_line_count,
            width,
            gutter_width,
            line_color,
        ));
        folded.extend(tail);
    }
    folded
}

fn summarize_additions_and_removals(additions: usize, removals: usize) -> String {
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
    parts.join(", ")
}

fn display_path(file_path: &str, cwd: Option<&str>, verbose: bool) -> String {
    if verbose {
        return file_path.to_owned();
    }

    let Some(cwd) = cwd else {
        return file_path.to_owned();
    };

    relative_to_cwd(cwd, file_path).unwrap_or_else(|| file_path.to_owned())
}

fn relative_to_cwd(cwd: &str, file_path: &str) -> Option<String> {
    let cwd_path = Path::new(cwd);
    let file_path = Path::new(file_path);
    let relative = file_path.strip_prefix(cwd_path).ok()?;
    Some(path_to_forward_slashes(relative))
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(lines: &[&str]) -> StructuredPatchHunk {
        StructuredPatchHunk {
            lines: lines.iter().map(|line| (*line).into()).collect(),
        }
    }

    #[test]
    fn file_edit_updated_uses_preview_hint_and_condensed_summary_rules() {
        let input = FileEditUpdatedInput {
            file_path: "src/lib.rs".into(),
            structured_patch: vec![hunk(&["+a", "-b"])],
            first_line: None,
            file_content: None,
            style_condensed: false,
            verbose: false,
            preview_hint: Some("Type /plan to inspect".into()),
            columns: 100,
        };
        assert_eq!(
            project_file_edit_updated(&input),
            FileEditUpdatedProjection::PreviewHint {
                hint: "Type /plan to inspect".into()
            }
        );

        let condensed = FileEditUpdatedInput {
            preview_hint: None,
            style_condensed: true,
            ..input
        };
        assert_eq!(
            project_file_edit_updated(&condensed),
            FileEditUpdatedProjection::SummaryOnly {
                summary: "Edit (src/lib.rs) · Added 1 line, removed 1 line".into()
            }
        );
    }

    #[test]
    fn file_edit_updated_counts_additions_and_removals_and_computes_width() {
        let input = FileEditUpdatedInput {
            file_path: "src/lib.rs".into(),
            structured_patch: vec![hunk(&["+a", "+b", "-c"])],
            first_line: Some("fn main()".into()),
            file_content: Some("old".into()),
            style_condensed: false,
            verbose: true,
            preview_hint: None,
            columns: 90,
        };
        let projection = project_file_edit_updated(&input);
        assert_eq!(
            projection,
            FileEditUpdatedProjection::Detailed {
                summary: "Edit (src/lib.rs) · Added 2 lines, removed 1 line".into(),
                diff_width: 78,
                fold_long_runs: false,
                file_path: "src/lib.rs".into(),
                first_line: Some("fn main()".into()),
                file_content: Some("old".into()),
                structured_patch: vec![hunk(&["+a", "+b", "-c"])],
            }
        );
    }

    #[test]
    fn file_edit_rejected_handles_condensed_write_and_diff_branches() {
        let condensed = FileEditRejectedInput {
            file_path: "C:/repo/src/lib.rs".into(),
            operation: FileEditOperation::Update,
            patch: vec![hunk(&["+a"])],
            first_line: None,
            file_content: None,
            content: None,
            style_condensed: true,
            verbose: false,
            columns: 100,
            cwd: Some("C:/repo".into()),
        };
        assert_eq!(
            project_file_edit_rejected(&condensed),
            FileEditRejectedProjection::SummaryOnly {
                summary: "User rejected update to src/lib.rs".into()
            }
        );

        let write = FileEditRejectedInput {
            file_path: "C:/repo/src/new.rs".into(),
            operation: FileEditOperation::Write,
            patch: Vec::new(),
            first_line: None,
            file_content: None,
            content: Some(
                (1..=12)
                    .map(|i| format!("line-{i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            style_condensed: false,
            verbose: false,
            columns: 80,
            cwd: Some("C:/repo".into()),
        };
        match project_file_edit_rejected(&write) {
            FileEditRejectedProjection::WritePreview {
                summary,
                preview,
                preview_width,
                hidden_line_count,
                ..
            } => {
                assert_eq!(summary, "User rejected write to src/new.rs");
                assert_eq!(preview.lines().count(), 10);
                assert_eq!(preview_width, 68);
                assert_eq!(hidden_line_count, 2);
            }
            other => panic!("expected WritePreview, got {other:?}"),
        }

        let diff = FileEditRejectedInput {
            file_path: "C:/repo/src/lib.rs".into(),
            operation: FileEditOperation::Update,
            patch: vec![hunk(&["+a", "-b"])],
            first_line: Some("fn main()".into()),
            file_content: Some("old".into()),
            content: None,
            style_condensed: false,
            verbose: false,
            columns: 90,
            cwd: Some("C:/repo".into()),
        };
        assert!(matches!(
            project_file_edit_rejected(&diff),
            FileEditRejectedProjection::DiffPreview {
                diff_width: 78,
                fold_long_runs: true,
                ..
            }
        ));
    }

    fn rendered_change_line(number: usize, text: &str, line_color: LineColor) -> RenderedLine {
        let sigil = match line_color {
            LineColor::Added | LineColor::AddedDimmed => '+',
            LineColor::Removed | LineColor::RemovedDimmed => '-',
            LineColor::None => ' ',
        };
        RenderedLine {
            gutter: format!("{number:>3} {sigil}"),
            content: vec![LineSegment {
                text: text.to_string(),
                word_color: WordColor::None,
            }],
            padding: String::new(),
            line_color,
            dim: false,
        }
    }

    fn rendered_line_text(line: &RenderedLine) -> String {
        line.content
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    #[test]
    fn long_added_run_uses_prompt_head_separator_and_tail() {
        let lines = (1..=30)
            .map(|line| rendered_change_line(line, &format!("added {line}"), LineColor::Added))
            .collect();

        let folded = fold_long_diff_runs(lines, 80);

        assert_eq!(folded.len(), 21);
        assert_eq!(rendered_line_text(&folded[9]), "added 10");
        assert!(rendered_line_text(&folded[10]).starts_with("──── (10 lines hidden) ─"));
        assert!(folded[10].dim);
        assert_eq!(folded[10].line_color, LineColor::Added);
        assert_eq!(rendered_line_text(&folded[11]), "added 21");
        assert!(folded[11].gutter.contains("21"));
        assert_eq!(rendered_line_text(&folded[20]), "added 30");
    }

    #[test]
    fn long_removed_run_folds_and_preserves_removed_tail_rows() {
        let lines = (1..=25)
            .map(|line| rendered_change_line(line, &format!("removed {line}"), LineColor::Removed))
            .collect();

        let folded = fold_long_diff_runs(lines, 72);

        assert_eq!(folded.len(), 21);
        assert!(rendered_line_text(&folded[10]).starts_with("──── (5 lines hidden) ─"));
        assert_eq!(folded[10].line_color, LineColor::Removed);
        assert_eq!(rendered_line_text(&folded[11]), "removed 16");
        assert_eq!(folded[11].line_color, LineColor::Removed);
        assert_eq!(rendered_line_text(&folded[20]), "removed 25");
    }

    #[test]
    fn added_and_removed_runs_fold_independently_across_context() {
        let mut lines = (1..=30)
            .map(|line| rendered_change_line(line, &format!("old {line}"), LineColor::Removed))
            .collect::<Vec<_>>();
        lines.push(rendered_change_line(31, "context", LineColor::None));
        lines.extend(
            (32..=61)
                .map(|line| rendered_change_line(line, &format!("new {line}"), LineColor::Added)),
        );

        let folded = fold_long_diff_runs(lines, 80);
        let texts = folded.iter().map(rendered_line_text).collect::<Vec<_>>();

        assert_eq!(folded.len(), 43);
        assert_eq!(
            folded
                .iter()
                .filter(|line| rendered_line_text(line).contains("lines hidden"))
                .map(|line| line.line_color)
                .collect::<Vec<_>>(),
            vec![LineColor::Removed, LineColor::Added]
        );
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.contains("(10 lines hidden)"))
                .count(),
            2
        );
        assert!(texts.iter().any(|text| text == "context"));
        assert!(!texts.iter().any(|text| text == "old 11"));
        assert!(!texts.iter().any(|text| text == "new 42"));
        assert!(texts.iter().any(|text| text == "old 21"));
        assert!(texts.iter().any(|text| text == "new 52"));
    }

    #[test]
    fn long_unchanged_run_uses_prompt_head_separator_and_tail() {
        let lines = (1..=30)
            .map(|line| rendered_change_line(line, &format!("context {line}"), LineColor::None))
            .collect();

        let folded = fold_long_diff_runs(lines, 80);

        assert_eq!(folded.len(), 21);
        assert_eq!(rendered_line_text(&folded[9]), "context 10");
        assert!(rendered_line_text(&folded[10]).starts_with("──── (10 lines hidden) ─"));
        assert!(folded[10].dim);
        assert_eq!(folded[10].line_color, LineColor::None);
        assert_eq!(rendered_line_text(&folded[11]), "context 21");
        assert!(folded[11].gutter.contains("21"));
        assert_eq!(rendered_line_text(&folded[20]), "context 30");
    }

    #[test]
    fn folding_obeys_prompt_threshold_boundary() {
        let short = (1..USER_PROMPT_FOLD_THRESHOLD_LINES)
            .map(|line| rendered_change_line(line, "short", LineColor::Added))
            .collect::<Vec<_>>();
        let boundary = (1..=USER_PROMPT_FOLD_THRESHOLD_LINES)
            .map(|line| rendered_change_line(line, "boundary", LineColor::AddedDimmed))
            .collect::<Vec<_>>();

        assert_eq!(fold_long_diff_runs(short.clone(), 80), short);
        let folded = fold_long_diff_runs(boundary, 80);
        assert_eq!(folded.len(), 21);
        assert!(rendered_line_text(&folded[10]).contains("(4 lines hidden)"));
        assert_eq!(folded[10].line_color, LineColor::AddedDimmed);
        assert_eq!(folded[11].line_color, LineColor::AddedDimmed);
    }

    #[test]
    fn context_splits_short_change_runs_instead_of_merging_them() {
        let mut lines = (1..=15)
            .map(|line| rendered_change_line(line, "first", LineColor::Added))
            .collect::<Vec<_>>();
        lines.push(rendered_change_line(16, "context", LineColor::None));
        lines.extend((17..=31).map(|line| rendered_change_line(line, "second", LineColor::Added)));

        let folded = fold_long_diff_runs(lines.clone(), 80);

        assert_eq!(folded, lines);
    }

    #[test]
    fn notebook_edit_rejected_uses_relative_paths_and_hides_preview_for_delete() {
        let replace = NotebookEditRejectedInput {
            notebook_path: "C:/repo/notebooks/demo.ipynb".into(),
            cell_id: Some("cell-1".into()),
            new_source: "print(1)".into(),
            cell_type: Some(NotebookCellType::Markdown),
            edit_mode: NotebookEditMode::Replace,
            verbose: false,
            cwd: Some("C:/repo".into()),
        };
        let projection = project_notebook_edit_rejected(&replace);
        assert_eq!(
            projection,
            NotebookEditRejectedProjection {
                summary: "User rejected replace cell in notebooks/demo.ipynb at cell cell-1".into(),
                preview: Some("print(1)".into()),
                preview_file_path: Some("file.md".into()),
            }
        );

        let delete = NotebookEditRejectedInput {
            edit_mode: NotebookEditMode::Delete,
            ..replace
        };
        let projection = project_notebook_edit_rejected(&delete);
        assert_eq!(projection.preview, None);
        assert_eq!(projection.preview_file_path, None);
    }
}
