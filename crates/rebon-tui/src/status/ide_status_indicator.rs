//! IDE-connection status row.
//!
//! The consumer feeds the resolved connection status and the current IDE
//! selection; the projection decides whether to show the indicator and
//! emits one of three branches:
//!
//! 1. Hidden (no selection, not connected, or an empty selection).
//! 2. `"⧉ <N> line(s) selected"` when the selection has highlighted
//!    text.
//! 3. `"⧉ In <basename(file_path)>"` when only a file path is selected.
//!
//! Pure logic implemented here:
//!
//! 1. The visibility decision.
//! 2. The line/file branch dispatch.
//! 3. The 1/N pluralization.
//! 4. The [`basename`] helper — a local copy that handles both `/` and
//!    `\\` separators.

/// IDE connection status. A missing status means "never show the
/// indicator for this IDE"; we model that as `None` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdeConnectionStatus {
    /// `'connected'` — IDE is fully connected.
    Connected,
    /// `'disconnected'` — IDE is configured but not connected.
    Disconnected,
}

/// The IDE selection the indicator reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdeSelection {
    /// Path of the file the cursor is in.
    pub file_path: Option<String>,
    /// Selected text (if any).
    pub text: Option<String>,
    /// How many lines the selection spans. Always present in the
    /// input; modelled as `u64` (default 0).
    pub line_count: u64,
}

/// Branch decision for the indicator, surfaced as an enum so the
/// consumer can dispatch on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdeStatusBranch {
    /// Indicator hidden.
    Hidden,
    /// Selection text branch: `"⧉ N line(s) selected"`.
    Selection {
        /// Pre-formatted text.
        text: String,
    },
    /// File-path branch: `"⧉ In <basename>"`.
    FileName {
        /// Pre-formatted text.
        text: String,
    },
}

/// Build the indicator branch from the resolved status and selection.
pub fn ide_status_indicator(
    status: Option<IdeConnectionStatus>,
    selection: Option<&IdeSelection>,
) -> IdeStatusBranch {
    let connected = matches!(status, Some(IdeConnectionStatus::Connected));
    let Some(sel) = selection else {
        return IdeStatusBranch::Hidden;
    };

    let has_file = sel
        .file_path
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_text =
        sel.text.as_deref().map(|s| !s.is_empty()).unwrap_or(false) && sel.line_count > 0;

    let should_show = connected && (has_file || has_text);
    if status.is_none() || !should_show {
        return IdeStatusBranch::Hidden;
    }

    if has_text {
        let unit = if sel.line_count == 1 { "line" } else { "lines" };
        return IdeStatusBranch::Selection {
            text: format!("\u{29c9} {} {} selected", sel.line_count, unit),
        };
    }
    if has_file {
        let path = sel.file_path.as_deref().unwrap_or("");
        return IdeStatusBranch::FileName {
            text: format!("\u{29c9} In {}", basename(path)),
        };
    }
    IdeStatusBranch::Hidden
}

/// Local `basename` for both Unix and Windows separators. Returns the
/// input unchanged if it has no separator.
pub fn basename(path: &str) -> &str {
    // Strip trailing separators (without removing the only char if path is "/")
    let trimmed = {
        let mut end = path.len();
        while end > 1 && matches!(path.as_bytes()[end - 1], b'/' | b'\\') {
            end -= 1;
        }
        &path[..end]
    };

    let last_slash = trimmed
        .rfind(|c: char| c == '/' || c == '\\')
        .map(|i| i + 1)
        .unwrap_or(0);
    &trimmed[last_slash..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(file: Option<&str>, text: Option<&str>, lines: u64) -> IdeSelection {
        IdeSelection {
            file_path: file.map(Into::into),
            text: text.map(Into::into),
            line_count: lines,
        }
    }

    #[test]
    fn hidden_when_no_selection() {
        assert_eq!(
            ide_status_indicator(Some(IdeConnectionStatus::Connected), None),
            IdeStatusBranch::Hidden
        );
    }

    #[test]
    fn hidden_when_status_none() {
        let s = sel(Some("/a/b.rs"), None, 0);
        assert_eq!(
            ide_status_indicator(None, Some(&s)),
            IdeStatusBranch::Hidden
        );
    }

    #[test]
    fn hidden_when_disconnected() {
        let s = sel(Some("/a/b.rs"), None, 0);
        assert_eq!(
            ide_status_indicator(Some(IdeConnectionStatus::Disconnected), Some(&s)),
            IdeStatusBranch::Hidden
        );
    }

    #[test]
    fn hidden_when_selection_is_empty() {
        let s = sel(None, None, 0);
        assert_eq!(
            ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s)),
            IdeStatusBranch::Hidden
        );
    }

    #[test]
    fn shows_file_name_branch() {
        let s = sel(Some("/home/user/project/main.rs"), None, 0);
        assert_eq!(
            ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s)),
            IdeStatusBranch::FileName {
                text: "\u{29c9} In main.rs".into()
            }
        );
    }

    #[test]
    fn shows_selection_singular() {
        let s = sel(Some("/home/user/main.rs"), Some("hi"), 1);
        assert_eq!(
            ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s)),
            IdeStatusBranch::Selection {
                text: "\u{29c9} 1 line selected".into()
            }
        );
    }

    #[test]
    fn shows_selection_plural() {
        let s = sel(Some("/home/user/main.rs"), Some("hi\nbye"), 2);
        assert_eq!(
            ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s)),
            IdeStatusBranch::Selection {
                text: "\u{29c9} 2 lines selected".into()
            }
        );
    }

    #[test]
    fn selection_branch_takes_priority_when_text_present() {
        // Both file_path and text/line_count present → selection branch wins.
        let s = sel(Some("/x/main.rs"), Some("foo"), 3);
        let branch = ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s));
        match branch {
            IdeStatusBranch::Selection { text } => assert!(text.contains("3 lines selected")),
            other => panic!("expected Selection, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_falls_back_to_file_name() {
        let s = sel(Some("/x/main.rs"), Some(""), 5);
        let branch = ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s));
        match branch {
            IdeStatusBranch::FileName { text } => assert!(text.contains("main.rs")),
            other => panic!("expected FileName, got {other:?}"),
        }
    }

    #[test]
    fn line_count_zero_falls_back_to_file_name() {
        let s = sel(Some("/x/main.rs"), Some("foo"), 0);
        let branch = ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s));
        match branch {
            IdeStatusBranch::FileName { .. } => {}
            other => panic!("expected FileName, got {other:?}"),
        }
    }

    #[test]
    fn basename_unix() {
        assert_eq!(basename("/a/b/c.rs"), "c.rs");
    }

    #[test]
    fn basename_windows() {
        assert_eq!(basename("C:\\a\\b\\c.rs"), "c.rs");
    }

    #[test]
    fn basename_no_slash() {
        assert_eq!(basename("c.rs"), "c.rs");
    }

    #[test]
    fn basename_strips_trailing_slash() {
        assert_eq!(basename("/a/b/"), "b");
    }

    #[test]
    fn basename_of_root_is_empty() {
        // Stripping trailing separators leaves the empty string once only
        // the single leading "/" remains.
        assert_eq!(basename("/"), "");
    }

    #[test]
    fn ide_indicator_uses_white_square_glyph() {
        let s = sel(Some("/x/y.rs"), None, 0);
        let branch = ide_status_indicator(Some(IdeConnectionStatus::Connected), Some(&s));
        match branch {
            IdeStatusBranch::FileName { text } => assert!(text.starts_with("\u{29c9}")),
            other => panic!("expected FileName, got {other:?}"),
        }
    }
}
