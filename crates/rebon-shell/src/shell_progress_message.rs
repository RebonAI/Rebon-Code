//! Projection for a shell command that is still running, or that has already
//! produced output.
//!
//! It renders either:
//!
//! * an empty-output running row: `Running…` plus an optional time display, or
//! * a tail/full output block plus metadata (`~N lines`, elapsed time,
//!   formatted byte size).
//!
//! Only the branching and the string projection live here; painting the row is
//! the caller's job.

use crate::shell_time_display::{format_file_size, project_shell_time_display, ShellTimeDisplay};

/// Most output lines kept in the streamed window (the last five, or fewer
/// when the output is shorter).
pub const MAX_PROGRESS_LINES: usize = 5;

/// Input bag for the shell progress projector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProgressInput {
    /// The currently streamed output window.
    pub output: String,
    /// The full output accumulated so far.
    pub full_output: String,
    /// Elapsed runtime in seconds.
    pub elapsed_time_seconds: Option<u64>,
    /// Estimated or exact total line count when known.
    pub total_lines: Option<usize>,
    /// Total byte count when known.
    pub total_bytes: Option<u64>,
    /// Timeout budget in milliseconds when present.
    pub timeout_ms: Option<u64>,
    /// Whether the verbose output style is in use.
    pub verbose: bool,
}

/// Shared metadata row from the output branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProgressMetadata {
    /// `~N lines` or `+N lines` suffix text.
    pub line_status: Option<String>,
    /// Optional elapsed/timeout display.
    pub time_display: Option<ShellTimeDisplay>,
    /// Formatted total-byte label.
    pub total_bytes_label: Option<String>,
}

/// Empty-output running branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyShellProgressDisplay {
    /// The leading status text (`Running…`).
    pub status_text: &'static str,
    /// Optional elapsed/timeout display.
    pub time_display: Option<ShellTimeDisplay>,
}

/// Non-empty output branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProgressOutputDisplay {
    /// The visible output body.
    pub display_lines: String,
    /// The fixed box height in non-verbose mode.
    pub display_height: Option<usize>,
    /// Metadata row rendered under the output block.
    pub metadata: ShellProgressMetadata,
}

/// Which of the two rows a shell progress message projects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellProgressDisplay {
    /// Empty-output running branch.
    Empty(EmptyShellProgressDisplay),
    /// Non-empty output branch.
    Output(ShellProgressOutputDisplay),
}

/// Project one shell progress message into the row a renderer paints.
pub fn project_shell_progress_message(input: &ShellProgressInput) -> ShellProgressDisplay {
    let stripped_full_output = strip_ansi(input.full_output.trim());
    let stripped_output = strip_ansi(input.output.trim());
    let lines: Vec<&str> = stripped_output
        .split('\n')
        .filter(|line| !line.is_empty())
        .collect();
    let time_display = project_shell_time_display(input.elapsed_time_seconds, input.timeout_ms);

    if lines.is_empty() {
        return ShellProgressDisplay::Empty(EmptyShellProgressDisplay {
            status_text: "Running\u{2026}",
            time_display,
        });
    }

    let display_lines = if input.verbose {
        stripped_full_output
    } else {
        lines
            .iter()
            .skip(lines.len().saturating_sub(MAX_PROGRESS_LINES))
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    };

    let extra_lines = input
        .total_lines
        .map(|count| count.saturating_sub(MAX_PROGRESS_LINES))
        .unwrap_or(0);

    // An exact total is only trusted when a byte count accompanies it; a bare
    // total is treated as a lower bound and reported as the overflow instead.
    let line_status =
        if !input.verbose && input.total_bytes.is_some() && input.total_lines.is_some() {
            input.total_lines.map(|count| format!("~{count} lines"))
        } else if !input.verbose && extra_lines > 0 {
            Some(format!("+{extra_lines} lines"))
        } else {
            None
        };

    ShellProgressDisplay::Output(ShellProgressOutputDisplay {
        display_lines,
        display_height: if input.verbose {
            None
        } else {
            Some(MAX_PROGRESS_LINES.min(lines.len()))
        },
        metadata: ShellProgressMetadata {
            line_status,
            time_display,
            total_bytes_label: input.total_bytes.map(format_file_size),
        },
    })
}

fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut out = String::with_capacity(text.len());

    while i < bytes.len() {
        match bytes[i] {
            0x1b if i + 1 < bytes.len() && bytes[i + 1] == b'[' => {
                i += 2;
                while i < bytes.len() {
                    if (0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            0x1b if i + 1 < bytes.len() && bytes[i + 1] == b']' => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                let ch = text[i..]
                    .chars()
                    .next()
                    .expect("index always on char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(output: &str, full_output: &str) -> ShellProgressInput {
        ShellProgressInput {
            output: output.to_string(),
            full_output: full_output.to_string(),
            elapsed_time_seconds: None,
            total_lines: None,
            total_bytes: None,
            timeout_ms: None,
            verbose: false,
        }
    }

    #[test]
    fn empty_output_branch_shows_running_with_time() {
        let mut input = input("", "");
        input.elapsed_time_seconds = Some(12);
        let display = project_shell_progress_message(&input);
        assert_eq!(
            display,
            ShellProgressDisplay::Empty(EmptyShellProgressDisplay {
                status_text: "Running\u{2026}",
                time_display: Some(ShellTimeDisplay {
                    text: "(12s)".to_string()
                }),
            })
        );
    }

    #[test]
    fn non_verbose_shows_last_five_lines_only() {
        let input = input("1\n2\n3\n4\n5\n6", "1\n2\n3\n4\n5\n6");
        let display = project_shell_progress_message(&input);
        let ShellProgressDisplay::Output(output) = display else {
            panic!("expected output branch");
        };
        assert_eq!(output.display_lines, "2\n3\n4\n5\n6");
        assert_eq!(output.display_height, Some(5));
    }

    #[test]
    fn verbose_shows_stripped_full_output() {
        let mut input = input("1\n2", "\u{1b}[31m1\n2\u{1b}[0m");
        input.verbose = true;
        let display = project_shell_progress_message(&input);
        let ShellProgressDisplay::Output(output) = display else {
            panic!("expected output branch");
        };
        assert_eq!(output.display_lines, "1\n2");
        assert_eq!(output.display_height, None);
    }

    #[test]
    fn line_status_prefers_estimated_total_when_total_lines_and_bytes_present() {
        let mut input = input("1\n2\n3\n4\n5\n6", "1\n2\n3\n4\n5\n6");
        input.total_lines = Some(2000);
        input.total_bytes = Some(1024);
        let display = project_shell_progress_message(&input);
        let ShellProgressDisplay::Output(output) = display else {
            panic!("expected output branch");
        };
        assert_eq!(output.metadata.line_status.as_deref(), Some("~2000 lines"));
        assert_eq!(output.metadata.total_bytes_label.as_deref(), Some("1KB"));
    }

    #[test]
    fn line_status_uses_extra_lines_when_only_total_lines_present() {
        let mut input = input("1\n2\n3\n4\n5\n6", "1\n2\n3\n4\n5\n6");
        input.total_lines = Some(7);
        let display = project_shell_progress_message(&input);
        let ShellProgressDisplay::Output(output) = display else {
            panic!("expected output branch");
        };
        assert_eq!(output.metadata.line_status.as_deref(), Some("+2 lines"));
    }

    #[test]
    fn line_status_hidden_in_verbose_mode() {
        let mut input = input("1\n2\n3\n4\n5\n6", "1\n2\n3\n4\n5\n6");
        input.total_lines = Some(7);
        input.verbose = true;
        let display = project_shell_progress_message(&input);
        let ShellProgressDisplay::Output(output) = display else {
            panic!("expected output branch");
        };
        assert_eq!(output.metadata.line_status, None);
    }

    #[test]
    fn metadata_threads_time_display() {
        let mut input = input("1", "1");
        input.elapsed_time_seconds = Some(8);
        input.timeout_ms = Some(120_000);
        let display = project_shell_progress_message(&input);
        let ShellProgressDisplay::Output(output) = display else {
            panic!("expected output branch");
        };
        assert_eq!(
            output.metadata.time_display,
            Some(ShellTimeDisplay {
                text: "(8s \u{00b7} timeout 2m)".to_string()
            })
        );
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_sequences() {
        let text = "\u{1b}[31mred\u{1b}[0m \u{1b}]8;;https://a.com\u{7}x\u{1b}]8;;\u{7}";
        assert_eq!(strip_ansi(text), "red x");
    }

    #[test]
    fn blank_lines_do_not_count_as_output_lines() {
        let input = input("\n\n", "\n\n");
        let display = project_shell_progress_message(&input);
        assert!(matches!(display, ShellProgressDisplay::Empty(_)));
    }
}
