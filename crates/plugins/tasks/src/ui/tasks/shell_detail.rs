//! Shell detail dialog projection.
//!
//! Three pure pieces:
//!
//! 1. The status / runtime / command rows (a small projector that
//!    accepts a `ShellDetailInput` and returns the rendered fields).
//! 2. The output-tail line splitter — `extract_tail_lines` — which
//!    pulls the last 10 lines from a `tail`-bytes string and reports
//!    whether the output is incomplete (because `bytes_total` exceeds
//!    the content length).
//! 3. The keyboard reducer (same shape as the async-agent dialog).
//!
//! Reading the output file's tail is the consumer's responsibility.

use crate::ui::tasks::common::TaskStatus;

/// Tail-byte cap: how much of the output file the consumer reads.
pub const SHELL_DETAIL_TAIL_BYTES: usize = 8192;

/// Visible-line cap when extracting the tail. The extraction loop runs
/// at most this many times, stopping early once `pos <= 0`.
pub const VISIBLE_TAIL_LINES: usize = 10;

/// Minimum interior height of the output box.
pub const OUTPUT_BOX_HEIGHT: usize = 12;

/// Pre-built input for [`build_shell_detail`].
#[derive(Debug, Clone)]
pub struct ShellDetailInput {
    /// True when the shell task kind is `monitor`.
    pub is_monitor: bool,
    /// The shell command.
    pub command: String,
    /// The shell task's status.
    pub status: TaskStatus,
    /// Start time in ms.
    pub start_time_ms: u64,
    /// End time in ms (None when still running — we use `now_ms`
    /// instead).
    pub end_time_ms: Option<u64>,
    /// `now_ms` for the running fallback.
    pub now_ms: u64,
    /// The exit code, when the shell reported one.
    pub exit_code: Option<i32>,
    /// True when the dialog can stop the shell. Drives the "x: stop" hint.
    pub can_kill: bool,
    /// True when the dialog can go back to a parent view. Drives the
    /// "←: go back" hint.
    pub can_back: bool,
}

const COMMAND_TRUNCATE: usize = 280;

/// Projected shell detail dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellDetail {
    /// `"Shell details"` or `"Monitor details"`.
    pub title: String,
    /// `"Status: {status}{ (exit code: …)}"` row.
    pub status_row: ShellStatusRow,
    /// Runtime in milliseconds — the consumer formats it.
    pub runtime_ms: u64,
    /// `"Command:"` or `"Script:"`.
    pub command_label: String,
    /// Truncated command body.
    pub command_body: String,
    /// Byline hints.
    pub byline: ShellByline,
}

/// Status row parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellStatusRow {
    /// Status string (e.g. `"running"`).
    pub status: String,
    /// Color name (`"background"` / `"success"` / `"error"`).
    pub color: String,
    /// Pre-formatted exit-code suffix when present (e.g. `" (exit
    /// code: 0)"`). `None` when no exit code is reported.
    pub exit_code_suffix: Option<String>,
}

/// Byline hints. Same shape as other dialog bylines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellByline {
    /// Show the `← go back` hint.
    pub show_back: bool,
    /// Always shown.
    pub show_close: bool,
    /// Show the `x stop` hint.
    pub show_stop: bool,
}

fn truncate_to_width(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

/// Build the projected shell detail.
pub fn build_shell_detail(input: &ShellDetailInput) -> ShellDetail {
    let title = if input.is_monitor {
        "Monitor details"
    } else {
        "Shell details"
    };

    let exit_code_suffix = input.exit_code.map(|code| format!(" (exit code: {code})"));

    let color = match input.status {
        TaskStatus::Running | TaskStatus::Pending => "background",
        TaskStatus::Completed => "success",
        TaskStatus::Failed | TaskStatus::Killed => "error",
    };

    let status_row = ShellStatusRow {
        status: input.status.as_str().to_owned(),
        color: color.to_owned(),
        exit_code_suffix,
    };

    let end = input.end_time_ms.unwrap_or(input.now_ms);
    let runtime_ms = end.saturating_sub(input.start_time_ms);

    let command_label = if input.is_monitor {
        "Script:"
    } else {
        "Command:"
    }
    .to_owned();
    let command_body = truncate_to_width(&input.command, COMMAND_TRUNCATE);

    let byline = ShellByline {
        show_back: input.can_back,
        show_close: true,
        show_stop: input.status == TaskStatus::Running && input.can_kill,
    };

    ShellDetail {
        title: title.to_owned(),
        status_row,
        runtime_ms,
        command_label,
        command_body,
        byline,
    }
}

/// Result of [`extract_tail_lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailLines {
    /// Up to [`VISIBLE_TAIL_LINES`] lines from the tail. The trailing
    /// (incomplete) partial line is included if present.
    pub lines: Vec<String>,
    /// True when `bytes_total > content.len()` — the consumer should
    /// show "X lines of Y file size" instead of just "X lines".
    pub is_incomplete: bool,
}

/// Extract the last [`VISIBLE_TAIL_LINES`] lines from a content
/// string.
///
/// The algorithm:
///
/// 1. Walk backwards from the end of the content, collecting `\n`
///    positions until we have 10 (or run out of content).
/// 2. Reverse to chronological order.
/// 3. Slice content into the resulting line ranges.
/// 4. Drop empty lines.
pub fn extract_tail_lines(content: &str, bytes_total: usize) -> TailLines {
    if content.is_empty() {
        return TailLines {
            lines: Vec::new(),
            is_incomplete: bytes_total > 0,
        };
    }
    let bytes = content.as_bytes();
    let mut starts: Vec<usize> = Vec::with_capacity(VISIBLE_TAIL_LINES);
    let mut pos: i64 = bytes.len() as i64;
    for _ in 0..VISIBLE_TAIL_LINES {
        if pos <= 0 {
            break;
        }
        // Last `\n` at or before `pos - 1`.
        let upper = (pos - 1) as usize;
        let prev = bytes[..=upper].iter().rposition(|&b| b == b'\n');
        let prev_i: i64 = match prev {
            Some(p) => p as i64,
            None => -1,
        };
        starts.push((prev_i + 1) as usize);
        pos = prev_i;
    }
    starts.reverse();
    let is_incomplete = bytes_total > content.len();
    let mut lines: Vec<String> = Vec::new();
    for i in 0..starts.len() {
        let start = starts[i];
        let end = if i < starts.len() - 1 {
            starts[i + 1].saturating_sub(1)
        } else {
            content.len()
        };
        if start > end {
            continue;
        }
        let line = &content[start..end];
        if !line.is_empty() {
            lines.push(line.to_owned());
        }
    }
    TailLines {
        lines,
        is_incomplete,
    }
}

/// Format the "Showing N lines[ of {size}]" tail summary line. The size
/// formatting is the
/// consumer's responsibility — we accept it as a string parameter.
pub fn format_tail_summary(line_count: usize, formatted_size: Option<&str>) -> String {
    let head = format!("Showing {line_count} lines");
    if let Some(size) = formatted_size {
        format!("{head} of {size}")
    } else {
        head
    }
}

/// Reducer event for the shell-detail dialog. Same surface as the
/// async-agent dialog reducer.
pub type ShellDetailEvent = crate::ui::tasks::async_agent_detail::AsyncAgentDetailEvent;

/// Reducer action for the shell-detail dialog.
pub type ShellDetailAction = crate::ui::tasks::async_agent_detail::AsyncAgentDetailAction;

/// Reducer for the shell-detail dialog.
pub fn handle_shell_event(
    event: ShellDetailEvent,
    status: TaskStatus,
    can_back: bool,
    can_kill: bool,
) -> ShellDetailAction {
    crate::ui::tasks::async_agent_detail::handle_async_agent_event(
        event, status, can_back, can_kill,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(status: TaskStatus) -> ShellDetailInput {
        ShellDetailInput {
            is_monitor: false,
            command: "ls -la".into(),
            status,
            start_time_ms: 1_000,
            end_time_ms: Some(3_500),
            now_ms: 5_000,
            exit_code: Some(0),
            can_kill: false,
            can_back: false,
        }
    }

    #[test]
    fn title_monitor_vs_shell() {
        let mut i = input(TaskStatus::Running);
        let d = build_shell_detail(&i);
        assert_eq!(d.title, "Shell details");
        i.is_monitor = true;
        let d = build_shell_detail(&i);
        assert_eq!(d.title, "Monitor details");
    }

    #[test]
    fn command_label_monitor_vs_shell() {
        let mut i = input(TaskStatus::Running);
        let d = build_shell_detail(&i);
        assert_eq!(d.command_label, "Command:");
        i.is_monitor = true;
        let d = build_shell_detail(&i);
        assert_eq!(d.command_label, "Script:");
    }

    #[test]
    fn status_row_color_table() {
        let cases = [
            (TaskStatus::Running, "background"),
            (TaskStatus::Pending, "background"),
            (TaskStatus::Completed, "success"),
            (TaskStatus::Failed, "error"),
            (TaskStatus::Killed, "error"),
        ];
        for (status, color) in cases {
            let d = build_shell_detail(&input(status));
            assert_eq!(d.status_row.color, color, "status: {:?}", status);
        }
    }

    #[test]
    fn status_row_exit_code_suffix() {
        let mut i = input(TaskStatus::Completed);
        i.exit_code = Some(2);
        let d = build_shell_detail(&i);
        assert_eq!(
            d.status_row.exit_code_suffix.as_deref(),
            Some(" (exit code: 2)")
        );
        i.exit_code = None;
        let d = build_shell_detail(&i);
        assert!(d.status_row.exit_code_suffix.is_none());
    }

    #[test]
    fn runtime_uses_end_time_when_present() {
        let i = input(TaskStatus::Completed);
        let d = build_shell_detail(&i);
        // 3500 - 1000
        assert_eq!(d.runtime_ms, 2_500);
    }

    #[test]
    fn runtime_uses_now_when_no_end_time() {
        let mut i = input(TaskStatus::Running);
        i.end_time_ms = None;
        let d = build_shell_detail(&i);
        // now=5000, start=1000 → 4000
        assert_eq!(d.runtime_ms, 4_000);
    }

    #[test]
    fn command_truncation() {
        let mut i = input(TaskStatus::Running);
        i.command = "x".repeat(400);
        let d = build_shell_detail(&i);
        assert!(d.command_body.chars().count() <= COMMAND_TRUNCATE);
        assert!(d.command_body.ends_with('…'));
    }

    #[test]
    fn byline_running_with_kill_back() {
        let mut i = input(TaskStatus::Running);
        i.can_kill = true;
        i.can_back = true;
        let d = build_shell_detail(&i);
        assert!(d.byline.show_back);
        assert!(d.byline.show_close);
        assert!(d.byline.show_stop);
    }

    #[test]
    fn byline_completed_hides_stop() {
        let mut i = input(TaskStatus::Completed);
        i.can_kill = true;
        let d = build_shell_detail(&i);
        assert!(!d.byline.show_stop);
    }

    #[test]
    fn extract_tail_empty_content_complete() {
        let r = extract_tail_lines("", 0);
        assert!(r.lines.is_empty());
        assert!(!r.is_incomplete);
    }

    #[test]
    fn extract_tail_empty_content_incomplete() {
        let r = extract_tail_lines("", 100);
        assert!(r.lines.is_empty());
        assert!(r.is_incomplete);
    }

    #[test]
    fn extract_tail_few_lines_no_trailing_newline() {
        let r = extract_tail_lines("a\nb\nc", 5);
        // No trailing newline → "c" included
        assert_eq!(r.lines, vec!["a", "b", "c"]);
        assert!(!r.is_incomplete);
    }

    #[test]
    fn extract_tail_caps_at_visible() {
        let content: String = (0..15)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = extract_tail_lines(&content, content.len());
        // Last 10 only
        assert_eq!(r.lines.len(), VISIBLE_TAIL_LINES);
        assert_eq!(r.lines[0], "line5");
        assert_eq!(r.lines[9], "line14");
    }

    #[test]
    fn extract_tail_marks_incomplete_when_bytes_exceed() {
        let r = extract_tail_lines("a\nb\n", 100);
        assert!(r.is_incomplete);
    }

    #[test]
    fn extract_tail_drops_empty_slices() {
        // Two consecutive newlines → empty middle line dropped
        let r = extract_tail_lines("a\n\nb", 4);
        assert_eq!(r.lines, vec!["a", "b"]);
    }

    #[test]
    fn tail_summary_no_size() {
        assert_eq!(format_tail_summary(5, None), "Showing 5 lines");
    }

    #[test]
    fn tail_summary_with_size() {
        assert_eq!(
            format_tail_summary(7, Some("12 KB")),
            "Showing 7 lines of 12 KB"
        );
    }

    #[test]
    fn handle_event_passthrough() {
        use crate::ui::tasks::async_agent_detail::{AsyncAgentDetailAction, AsyncAgentDetailEvent};
        assert_eq!(
            handle_shell_event(
                AsyncAgentDetailEvent::Space,
                TaskStatus::Running,
                false,
                false
            ),
            AsyncAgentDetailAction::Done
        );
    }
}
