//! Shell-mode progress wrapper.
//!
//! The projection takes the typed input string plus an optional progress
//! snapshot and describes two stacked rows:
//!
//! 1. The bash-input row, whose text is
//!    `"<bash-input>{input}</bash-input>"`.
//! 2. Either the shell-progress row (when a progress snapshot is
//!    present) or the bash tool's progress placeholder.
//!
//! Pure logic: just the input wrapping and the progress branch shape.
//! Both rows are rendered by other layers; we expose the layout the
//! consumer dispatches on.

/// A shell-progress snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProgress {
    /// Full output captured so far.
    pub full_output: String,
    /// Truncated/streamed output the message currently shows.
    pub output: String,
    /// Elapsed wall time in seconds.
    pub elapsed_time_seconds: u64,
    /// Total line count of [`full_output`](Self::full_output).
    pub total_lines: u64,
}

/// Layout decision for the two stacked rows. The consumer renders the
/// bash-input row first, then the progress branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashModeProgressLayout {
    /// Wrapped input text:
    /// `"<bash-input>{input}</bash-input>"`.
    pub bash_input_text: String,
    /// `true` if the consumer should render the shell-progress row,
    /// `false` if it should render the bash tool's progress placeholder.
    pub has_progress: bool,
    /// Snapshot of the progress when present.
    pub progress: Option<ShellProgress>,
    /// `verbose` flag passed through to the inner message renderer.
    pub verbose: bool,
}

/// Build the two-row layout from the input and the optional progress.
pub fn bash_mode_progress_layout(
    input: &str,
    progress: Option<ShellProgress>,
    verbose: bool,
) -> BashModeProgressLayout {
    BashModeProgressLayout {
        bash_input_text: format!("<bash-input>{input}</bash-input>"),
        has_progress: progress.is_some(),
        progress,
        verbose,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_progress() -> ShellProgress {
        ShellProgress {
            full_output: "all".into(),
            output: "tail".into(),
            elapsed_time_seconds: 5,
            total_lines: 10,
        }
    }

    #[test]
    fn wraps_input_in_bash_input_tag() {
        let layout = bash_mode_progress_layout("ls -la", None, false);
        assert_eq!(layout.bash_input_text, "<bash-input>ls -la</bash-input>");
    }

    #[test]
    fn empty_input_still_wraps() {
        let layout = bash_mode_progress_layout("", None, false);
        assert_eq!(layout.bash_input_text, "<bash-input></bash-input>");
    }

    #[test]
    fn input_with_special_chars_is_left_alone() {
        let layout = bash_mode_progress_layout("echo \"hi\" && pwd", None, false);
        assert_eq!(
            layout.bash_input_text,
            "<bash-input>echo \"hi\" && pwd</bash-input>"
        );
    }

    #[test]
    fn null_progress_sets_has_progress_false() {
        let layout = bash_mode_progress_layout("cmd", None, false);
        assert!(!layout.has_progress);
        assert!(layout.progress.is_none());
    }

    #[test]
    fn some_progress_sets_has_progress_true() {
        let layout = bash_mode_progress_layout("cmd", Some(sample_progress()), false);
        assert!(layout.has_progress);
        assert_eq!(layout.progress, Some(sample_progress()));
    }

    #[test]
    fn verbose_flag_is_passed_through() {
        let layout = bash_mode_progress_layout("cmd", None, true);
        assert!(layout.verbose);
        let layout = bash_mode_progress_layout("cmd", None, false);
        assert!(!layout.verbose);
    }

    #[test]
    fn progress_payload_is_preserved_exactly() {
        let p = sample_progress();
        let layout = bash_mode_progress_layout("cmd", Some(p.clone()), true);
        let got = layout.progress.unwrap();
        assert_eq!(got.full_output, "all");
        assert_eq!(got.output, "tail");
        assert_eq!(got.elapsed_time_seconds, 5);
        assert_eq!(got.total_lines, 10);
    }
}
