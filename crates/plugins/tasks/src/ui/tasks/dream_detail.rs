//! Dream / memory-consolidation detail dialog projection.
//!
//! The detail view shows:
//!
//! 1. A subtitle line `"{elapsed} · reviewing {n} session[s][ · {m}
//!    file[s] touched]"`.
//! 2. A status line: `Status: running` / `Status: completed` /
//!    `Status: error`.
//! 3. The last `VISIBLE_TURNS = 6` non-empty turns; earlier turns
//!    collapse to `"({n} earlier turn[s])"`.
//! 4. A keyboard reducer (`space → done`, `left → back`, `x → kill`)
//!    and a byline.

use crate::ui::tasks::common::{SemanticColor, TaskStatus};

/// Visible-turn cap.
pub const VISIBLE_TURNS: usize = 6;

/// One turn of dream output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamTurn {
    /// The turn's text output.
    pub text: String,
    /// Tool calls made during the turn.
    pub tool_use_count: u64,
}

/// Pre-built input for [`build_dream_detail`].
#[derive(Debug, Clone)]
pub struct DreamDetailInput {
    /// The dream task's status.
    pub status: TaskStatus,
    /// Pre-formatted elapsed-time string.
    pub elapsed_time: String,
    /// Number of sessions being reviewed.
    pub sessions_reviewing: u64,
    /// Number of files touched so far.
    pub files_touched: u64,
    /// The dream's turns, in chronological order.
    pub turns: Vec<DreamTurn>,
    /// True when the dialog can go back to a parent view.
    pub can_back: bool,
    /// True when the dialog can stop the task.
    pub can_kill: bool,
}

/// Projected dream detail. Plain strings + enum variants — no renderer
/// primitives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamDetail {
    /// `"Memory consolidation"` literal.
    pub title: String,
    /// Subtitle string.
    pub subtitle: String,
    /// `Status: …` projection.
    pub status_line: DreamStatusLine,
    /// Either trimmed turn list or the placeholder line.
    pub turns: DreamTurnsBlock,
    /// Byline hints.
    pub byline: DreamByline,
}

/// Status line shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamStatusLine {
    /// `task.status` as the user-visible label. Always lowercase.
    pub label: String,
    /// Color the label is rendered with.
    pub color: SemanticColor,
}

/// Either a list of visible turns (with optional "X earlier turn"
/// header) or the placeholder line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DreamTurnsBlock {
    /// Placeholder line — `"Starting…"` (running) or `"(no text
    /// output)"` (terminal status with no turns).
    Placeholder(String),
    /// At least one visible turn.
    Visible {
        /// Number of earlier turns suppressed (zero when nothing was
        /// trimmed).
        hidden: u64,
        /// The trimmed list of visible turns (most recent
        /// `VISIBLE_TURNS`, in chronological order).
        shown: Vec<DreamTurn>,
    },
}

/// Byline hints. Same shape as the async-agent byline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DreamByline {
    /// Show the `← go back` hint.
    pub show_back: bool,
    /// Always shown.
    pub show_close: bool,
    /// Show the `x stop` hint.
    pub show_stop: bool,
}

/// Pluralize a noun by appending `s` unless `n == 1`. Public so the
/// test matrix can pin the rule.
pub fn plural(n: u64, noun: &str) -> String {
    if n == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
    }
}

/// Reducer event for the dream detail dialog. Same surface as the
/// async-agent dialog reducer.
pub type DreamDetailEvent = crate::ui::tasks::async_agent_detail::AsyncAgentDetailEvent;

/// Reducer action for the dream detail dialog.
pub type DreamDetailAction = crate::ui::tasks::async_agent_detail::AsyncAgentDetailAction;

/// Reducer for the dream-detail dialog. The keyboard precedence is
/// identical to the async-agent reducer.
pub fn handle_dream_event(
    event: DreamDetailEvent,
    status: TaskStatus,
    can_back: bool,
    can_kill: bool,
) -> DreamDetailAction {
    crate::ui::tasks::async_agent_detail::handle_async_agent_event(
        event, status, can_back, can_kill,
    )
}

/// Project the dream detail dialog.
pub fn build_dream_detail(input: &DreamDetailInput) -> DreamDetail {
    let session_word = plural(input.sessions_reviewing, "session");
    let mut subtitle = format!(
        "{} · reviewing {} {session_word}",
        input.elapsed_time, input.sessions_reviewing
    );
    if input.files_touched > 0 {
        let file_word = plural(input.files_touched, "file");
        subtitle.push_str(&format!(" · {} {file_word} touched", input.files_touched));
    }

    let status_line = match input.status {
        TaskStatus::Running => DreamStatusLine {
            label: "running".into(),
            color: SemanticColor::Background,
        },
        TaskStatus::Completed => DreamStatusLine {
            label: "completed".into(),
            color: SemanticColor::Success,
        },
        TaskStatus::Failed => DreamStatusLine {
            label: "failed".into(),
            color: SemanticColor::Error,
        },
        TaskStatus::Killed => DreamStatusLine {
            label: "killed".into(),
            color: SemanticColor::Error,
        },
        TaskStatus::Pending => DreamStatusLine {
            label: "pending".into(),
            color: SemanticColor::Error,
        },
    };

    // Filter out empty-text turns then take last VISIBLE_TURNS.
    let visible: Vec<DreamTurn> = input
        .turns
        .iter()
        .filter(|t| !t.text.is_empty())
        .cloned()
        .collect();
    let total = visible.len();
    let take = total.min(VISIBLE_TURNS);
    let hidden = (total - take) as u64;
    let shown: Vec<DreamTurn> = visible.into_iter().skip(total - take).collect();

    let turns = if shown.is_empty() {
        let label = if input.status == TaskStatus::Running {
            "Starting…"
        } else {
            "(no text output)"
        };
        DreamTurnsBlock::Placeholder(label.into())
    } else {
        DreamTurnsBlock::Visible { hidden, shown }
    };

    let byline = DreamByline {
        show_back: input.can_back,
        show_close: true,
        show_stop: input.status == TaskStatus::Running && input.can_kill,
    };

    DreamDetail {
        title: "Memory consolidation".into(),
        subtitle,
        status_line,
        turns,
        byline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(text: &str, tools: u64) -> DreamTurn {
        DreamTurn {
            text: text.into(),
            tool_use_count: tools,
        }
    }

    fn input(status: TaskStatus) -> DreamDetailInput {
        DreamDetailInput {
            status,
            elapsed_time: "00:42".into(),
            sessions_reviewing: 3,
            files_touched: 0,
            turns: vec![],
            can_back: false,
            can_kill: false,
        }
    }

    #[test]
    fn plural_table() {
        assert_eq!(plural(0, "session"), "sessions");
        assert_eq!(plural(1, "session"), "session");
        assert_eq!(plural(2, "session"), "sessions");
        assert_eq!(plural(1, "file"), "file");
        assert_eq!(plural(7, "file"), "files");
    }

    #[test]
    fn subtitle_no_files() {
        let d = build_dream_detail(&input(TaskStatus::Running));
        assert_eq!(d.subtitle, "00:42 · reviewing 3 sessions");
    }

    #[test]
    fn subtitle_one_session_singular() {
        let mut i = input(TaskStatus::Running);
        i.sessions_reviewing = 1;
        let d = build_dream_detail(&i);
        assert_eq!(d.subtitle, "00:42 · reviewing 1 session");
    }

    #[test]
    fn subtitle_with_files() {
        let mut i = input(TaskStatus::Running);
        i.files_touched = 5;
        let d = build_dream_detail(&i);
        assert_eq!(d.subtitle, "00:42 · reviewing 3 sessions · 5 files touched");

        i.files_touched = 1;
        let d = build_dream_detail(&i);
        assert_eq!(d.subtitle, "00:42 · reviewing 3 sessions · 1 file touched");
    }

    #[test]
    fn status_line_color_table() {
        let cases = [
            (TaskStatus::Running, SemanticColor::Background, "running"),
            (TaskStatus::Completed, SemanticColor::Success, "completed"),
            (TaskStatus::Failed, SemanticColor::Error, "failed"),
            (TaskStatus::Killed, SemanticColor::Error, "killed"),
        ];
        for (status, color, label) in cases {
            let d = build_dream_detail(&input(status));
            assert_eq!(d.status_line.label, label);
            assert_eq!(d.status_line.color, color);
        }
    }

    #[test]
    fn turns_placeholder_running() {
        let d = build_dream_detail(&input(TaskStatus::Running));
        assert_eq!(d.turns, DreamTurnsBlock::Placeholder("Starting…".into()));
    }

    #[test]
    fn turns_placeholder_completed() {
        let d = build_dream_detail(&input(TaskStatus::Completed));
        assert_eq!(
            d.turns,
            DreamTurnsBlock::Placeholder("(no text output)".into())
        );
    }

    #[test]
    fn turns_filter_empty_text() {
        let mut i = input(TaskStatus::Running);
        i.turns = vec![
            turn("", 0),
            turn("first", 1),
            turn("", 0),
            turn("second", 0),
        ];
        let d = build_dream_detail(&i);
        assert_eq!(
            d.turns,
            DreamTurnsBlock::Visible {
                hidden: 0,
                shown: vec![turn("first", 1), turn("second", 0)],
            }
        );
    }

    #[test]
    fn turns_trim_to_visible_window() {
        let mut i = input(TaskStatus::Running);
        i.turns = (0..10).map(|n| turn(&format!("t{n}"), 0)).collect();
        let d = build_dream_detail(&i);
        match d.turns {
            DreamTurnsBlock::Visible { hidden, shown } => {
                assert_eq!(hidden, 4);
                assert_eq!(shown.len(), VISIBLE_TURNS);
                // Last 6 in chronological order
                assert_eq!(shown[0].text, "t4");
                assert_eq!(shown[5].text, "t9");
            }
            _ => panic!("expected visible block"),
        }
    }

    #[test]
    fn byline_running_with_kill_back() {
        let mut i = input(TaskStatus::Running);
        i.can_back = true;
        i.can_kill = true;
        let d = build_dream_detail(&i);
        assert!(d.byline.show_back);
        assert!(d.byline.show_close);
        assert!(d.byline.show_stop);
    }

    #[test]
    fn byline_completed_hides_stop() {
        let mut i = input(TaskStatus::Completed);
        i.can_kill = true;
        let d = build_dream_detail(&i);
        assert!(!d.byline.show_stop);
    }

    #[test]
    fn dream_event_dispatch_matches_async_agent() {
        // Just confirms the type alias wires up to the async-agent
        // reducer; the precedence table is tested there.
        use crate::ui::tasks::async_agent_detail::{AsyncAgentDetailAction, AsyncAgentDetailEvent};
        assert_eq!(
            handle_dream_event(
                AsyncAgentDetailEvent::Space,
                TaskStatus::Running,
                false,
                false
            ),
            AsyncAgentDetailAction::Done
        );
    }
}
