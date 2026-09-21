//! Diagnostics display projection: the severity symbol, the non-verbose
//! summary row, and the verbose per-file diagnostic lines.

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// Error severity.
    Error,
    /// Warning severity.
    Warning,
    /// Info severity.
    Info,
    /// Hint severity.
    Hint,
    /// Fallback severity.
    Other,
}

/// One diagnostic entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticEntry {
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Zero-based line.
    pub line: usize,
    /// Zero-based character.
    pub character: usize,
    /// Message text.
    pub message: String,
    /// Optional code.
    pub code: Option<String>,
    /// Optional source.
    pub source: Option<String>,
}

/// One diagnostic file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticFileDisplay {
    /// URI.
    pub uri: String,
    /// Diagnostics for that URI.
    pub diagnostics: Vec<DiagnosticEntry>,
}

/// Top-level diagnostics projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticsProjection {
    /// Non-verbose summary row.
    Summary {
        /// Total issue count.
        total_issues: usize,
        /// File count.
        file_count: usize,
        /// Singular/plural issue noun.
        issue_word: &'static str,
        /// Singular/plural file noun.
        file_word: &'static str,
    },
    /// Verbose per-file diagnostics.
    Verbose {
        /// Verbose file rows.
        files: Vec<VerboseDiagnosticFile>,
    },
}

/// Verbose per-file projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerboseDiagnosticFile {
    /// Path displayed to the user.
    pub display_path: String,
    /// Short marker naming the URI scheme the diagnostics came from.
    pub uri_suffix: String,
    /// Render-ready diagnostic lines.
    pub diagnostics: Vec<String>,
}

/// Symbol shown for a severity: `×` error, `⚠` warning, `i` info, `★` hint,
/// `•` otherwise.
pub fn severity_symbol(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "×",
        DiagnosticSeverity::Warning => "⚠",
        DiagnosticSeverity::Info => "i",
        DiagnosticSeverity::Hint => "★",
        DiagnosticSeverity::Other => "•",
    }
}

/// Projects the diagnostics into a summary row or a verbose per-file list;
/// `None` when there are no files. Verbose lines read
/// `<symbol> [Line <line+1>:<character+1>] <message> [<code>] (<source>)`,
/// with the bracketed code and parenthesised source omitted when absent.
pub fn project_diagnostics_display(
    files: &[DiagnosticFileDisplay],
    verbose: bool,
    cwd: &str,
) -> Option<DiagnosticsProjection> {
    if files.is_empty() {
        return None;
    }
    if verbose {
        return Some(DiagnosticsProjection::Verbose {
            files: files
                .iter()
                .map(|file| VerboseDiagnosticFile {
                    display_path: display_diagnostic_path(cwd, &file.uri),
                    uri_suffix: display_uri_suffix(&file.uri),
                    diagnostics: file
                        .diagnostics
                        .iter()
                        .map(|d| {
                            format!(
                                "{} [Line {}:{}] {}{}{}",
                                severity_symbol(d.severity),
                                d.line + 1,
                                d.character + 1,
                                d.message,
                                d.code
                                    .as_ref()
                                    .map(|c| format!(" [{c}]"))
                                    .unwrap_or_default(),
                                d.source
                                    .as_ref()
                                    .map(|s| format!(" ({s})"))
                                    .unwrap_or_default()
                            )
                        })
                        .collect(),
                })
                .collect(),
        });
    }
    let total_issues = files.iter().map(|f| f.diagnostics.len()).sum::<usize>();
    let file_count = files.len();
    Some(DiagnosticsProjection::Summary {
        total_issues,
        file_count,
        issue_word: if total_issues == 1 { "issue" } else { "issues" },
        file_word: if file_count == 1 { "file" } else { "files" },
    })
}

fn display_diagnostic_path(cwd: &str, uri: &str) -> String {
    let raw = uri
        .strip_prefix("file://")
        .or_else(|| uri.strip_prefix("_claude_fs_right:"))
        .unwrap_or(uri);
    let cwd_norm = cwd.replace('\\', "/");
    let raw_norm = raw.replace('\\', "/");
    raw_norm
        .strip_prefix(&(cwd_norm.clone() + "/"))
        .unwrap_or(raw_norm.as_str())
        .to_string()
}

fn display_uri_suffix(uri: &str) -> String {
    if uri.starts_with("file://") {
        "(file://)".to_string()
    } else if uri.starts_with("_claude_fs_right:") {
        "(claude_fs_right)".to_string()
    } else {
        format!("({})", uri.split(':').next().unwrap_or(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> DiagnosticFileDisplay {
        DiagnosticFileDisplay {
            uri: "file:///repo/a.ts".into(),
            diagnostics: vec![DiagnosticEntry {
                severity: DiagnosticSeverity::Error,
                line: 2,
                character: 4,
                message: "bad".into(),
                code: Some("E1".into()),
                source: Some("ts".into()),
            }],
        }
    }

    #[test]
    fn summary_projection_counts_issues_and_files() {
        let p = project_diagnostics_display(&[file()], false, "/repo").unwrap();
        assert_eq!(
            p,
            DiagnosticsProjection::Summary {
                total_issues: 1,
                file_count: 1,
                issue_word: "issue",
                file_word: "file",
            }
        );
    }

    #[test]
    fn verbose_projection_formats_path_suffix_and_diagnostic_line() {
        let p = project_diagnostics_display(&[file()], true, "/repo").unwrap();
        let DiagnosticsProjection::Verbose { files } = p else {
            panic!("expected verbose");
        };
        assert_eq!(files[0].display_path, "a.ts");
        assert_eq!(files[0].uri_suffix, "(file://)");
        assert!(files[0].diagnostics[0].contains("× [Line 3:5] bad"));
    }

    #[test]
    fn empty_files_hide_projection() {
        assert_eq!(project_diagnostics_display(&[], false, "/repo"), None);
    }
}
