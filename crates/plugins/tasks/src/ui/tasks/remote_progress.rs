//! Remote-session progress projection.
//!
//! Three exports:
//!
//! * [`format_review_stage_counts`] — the canonical
//!   "X found · Y
//!   verified · Z refuted · deduping" string used in both the one-line
//!   pill and the detail dialog. The two used to drift; this is the
//!   single source of truth.
//! * [`SmoothCount`] — a `+1`/frame
//!   tween reducer
//!   that's tested without the animation loop.
//! * [`format_remote_session_progress`] — returns
//!   a discriminated
//!   `RemoteProgressLine` so the consumer can render the different
//!   shapes (rainbow, completed pill, todo counts, …).

use crate::ui::tasks::common::{ReviewStage, TaskStatus};

/// Stage-appropriate counts line for a running review.
///
/// Canonical behaviour:
///
/// * No stage → `"{found} found · {verified} verified"` (pre-stage
///   orchestrator images don't write the stage field)
/// * `synthesizing` → `"{verified} verified · {refuted} refuted ·
///   deduping"` (refuted hidden when 0)
/// * `verifying` → `"{found} found · {verified} verified · {refuted}
///   refuted"` (refuted hidden when 0)
/// * `finding` → `"{found} found"` if `found > 0`, else `"finding"`
pub fn format_review_stage_counts(
    stage: Option<ReviewStage>,
    found: u64,
    verified: u64,
    refuted: u64,
) -> String {
    let Some(stage) = stage else {
        return format!("{found} found · {verified} verified");
    };
    match stage {
        ReviewStage::Synthesizing => {
            let mut parts: Vec<String> = vec![format!("{verified} verified")];
            if refuted > 0 {
                parts.push(format!("{refuted} refuted"));
            }
            parts.push("deduping".to_owned());
            parts.join(" · ")
        }
        ReviewStage::Verifying => {
            let mut parts: Vec<String> =
                vec![format!("{found} found"), format!("{verified} verified")];
            if refuted > 0 {
                parts.push(format!("{refuted} refuted"));
            }
            parts.join(" · ")
        }
        ReviewStage::Finding => {
            if found > 0 {
                format!("{found} found")
            } else {
                "finding".to_owned()
            }
        }
    }
}

/// Pure smooth-count tween.
///
/// The tween keeps two pieces of state across calls — the displayed
/// value and the tick of its last advance — and steps `displayed` up by
/// one when `target > displayed && time != last_tick`. The struct
/// applies this precedence:
///
/// 1. `snap || target < displayed` → snap to target.
/// 2. `target > displayed && time != last_tick` → `displayed += 1`,
///    `last_tick = time`.
/// 3. otherwise leave displayed/last_tick alone.
#[derive(Debug, Clone, Copy)]
pub struct SmoothCount {
    /// Currently displayed value.
    displayed: u64,
    /// Time of the last advance — `target` jumps that arrive on the
    /// same tick are ignored.
    last_tick: u64,
}

impl SmoothCount {
    /// Initialize at `target` with `time = 0`.
    pub fn new(target: u64) -> Self {
        Self {
            displayed: target,
            last_tick: 0,
        }
    }

    /// Step the tween. Returns the new displayed value.
    ///
    /// `snap=true` bypasses the
    /// tween and jumps straight to target.
    pub fn step(&mut self, target: u64, time: u64, snap: bool) -> u64 {
        if snap || target < self.displayed {
            self.displayed = target;
        } else if target > self.displayed && time != self.last_tick {
            self.displayed += 1;
            self.last_tick = time;
        }
        self.displayed
    }

    /// Currently displayed value.
    pub fn current(&self) -> u64 {
        self.displayed
    }
}

/// Discriminated remote-session progress line: three rainbow review
/// shapes (`completed`, `failed`, running) and the plain pills a
/// non-review session gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteProgressLine {
    /// Rainbow `ultrareview ready · shift+↓ to view` line. Produced for
    /// a remote review whose status is `TaskStatus::Completed`.
    ReviewReady,
    /// Rainbow `ultrareview · error` line. Produced for a remote review
    /// whose status is `TaskStatus::Failed`.
    ReviewFailed,
    /// Running rainbow line: `ultrareview · {tail}` where `tail` is
    /// `"setting up"` (no progress yet) or
    /// [`format_review_stage_counts`].
    ReviewRunning {
        /// The trailing string after `ultrareview · `. Either `"setting
        /// up"` or `format_review_stage_counts(...)`.
        tail: String,
    },
    /// Plain `done` pill: completed status, not a remote review.
    Done,
    /// Plain `error` pill: failed status, not a remote review.
    Error,
    /// Status string with trailing ellipsis when no todos exist:
    /// `"{status}…"`.
    StatusEllipsis(String),
    /// Todo progress: `"{completed}/{total}"`.
    TodoCounts {
        /// Number of completed todos.
        completed: u64,
        /// Total todos in the session.
        total: u64,
    },
}

/// Pre-built input shape for [`format_remote_session_progress`].
#[derive(Debug, Clone)]
pub struct RemoteSessionInput {
    /// The session's status.
    pub status: TaskStatus,
    /// Whether the session is a remote review. When true the rainbow review line is
    /// produced; when false the todo/done/error pills are produced.
    pub is_remote_review: bool,
    /// Number of completed todos in the session's todo list — the caller
    /// counts the entries whose status is completed.
    pub todo_completed: u64,
    /// Total number of todos in the session's todo list.
    pub todo_total: u64,
    /// Pre-projected review-progress shape — see
    /// [`ReviewProgressInput`].
    pub review: Option<ReviewProgressInput>,
}

/// Pre-built review-progress shape.
#[derive(Debug, Clone)]
pub struct ReviewProgressInput {
    /// Current review stage. Pre-stage orchestrator images leave this as `None`.
    pub stage: Option<ReviewStage>,
    /// Candidate bugs found.
    pub bugs_found: u64,
    /// Bugs verified.
    pub bugs_verified: u64,
    /// Bugs refuted.
    pub bugs_refuted: u64,
}

/// Project a remote session to its progress line.
///
/// Note that the smooth-count tween is the consumer's responsibility:
/// if the consumer wants the +1/frame animation it threads the
/// `SmoothCount` state itself and passes the *displayed* values into
/// this function via `review.bugs_*`. The model doesn't own the
/// animation clock.
pub fn format_remote_session_progress(input: &RemoteSessionInput) -> RemoteProgressLine {
    if input.is_remote_review {
        return match input.status {
            TaskStatus::Completed => RemoteProgressLine::ReviewReady,
            TaskStatus::Failed => RemoteProgressLine::ReviewFailed,
            _ => {
                let tail = match &input.review {
                    None => "setting up".to_owned(),
                    Some(p) => format_review_stage_counts(
                        p.stage,
                        p.bugs_found,
                        p.bugs_verified,
                        p.bugs_refuted,
                    ),
                };
                RemoteProgressLine::ReviewRunning { tail }
            }
        };
    }
    if input.status == TaskStatus::Completed {
        return RemoteProgressLine::Done;
    }
    if input.status == TaskStatus::Failed {
        return RemoteProgressLine::Error;
    }
    if input.todo_total == 0 {
        // Falls back to `{status}…` when there are no todos.
        return RemoteProgressLine::StatusEllipsis(format!("{}…", input.status));
    }
    RemoteProgressLine::TodoCounts {
        completed: input.todo_completed,
        total: input.todo_total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_counts_no_stage() {
        // Pre-stage orchestrator images
        assert_eq!(
            format_review_stage_counts(None, 3, 1, 0),
            "3 found · 1 verified"
        );
        assert_eq!(
            format_review_stage_counts(None, 0, 0, 99),
            "0 found · 0 verified",
            "refuted is ignored when stage is missing"
        );
    }

    #[test]
    fn stage_counts_finding_zero() {
        assert_eq!(
            format_review_stage_counts(Some(ReviewStage::Finding), 0, 0, 0),
            "finding"
        );
    }

    #[test]
    fn stage_counts_finding_nonzero() {
        assert_eq!(
            format_review_stage_counts(Some(ReviewStage::Finding), 7, 0, 0),
            "7 found"
        );
    }

    #[test]
    fn stage_counts_verifying_no_refuted() {
        assert_eq!(
            format_review_stage_counts(Some(ReviewStage::Verifying), 5, 2, 0),
            "5 found · 2 verified"
        );
    }

    #[test]
    fn stage_counts_verifying_with_refuted() {
        assert_eq!(
            format_review_stage_counts(Some(ReviewStage::Verifying), 5, 2, 1),
            "5 found · 2 verified · 1 refuted"
        );
    }

    #[test]
    fn stage_counts_synthesizing_no_refuted() {
        assert_eq!(
            format_review_stage_counts(Some(ReviewStage::Synthesizing), 9, 4, 0),
            "4 verified · deduping",
            "synthesizing drops the found count"
        );
    }

    #[test]
    fn stage_counts_synthesizing_with_refuted() {
        assert_eq!(
            format_review_stage_counts(Some(ReviewStage::Synthesizing), 9, 4, 2),
            "4 verified · 2 refuted · deduping"
        );
    }

    #[test]
    fn smooth_count_init_then_snap_down() {
        let mut sc = SmoothCount::new(5);
        assert_eq!(sc.current(), 5);
        // Target dropped → snap immediately
        assert_eq!(sc.step(2, 100, false), 2);
        assert_eq!(sc.current(), 2);
    }

    #[test]
    fn smooth_count_snap_flag() {
        let mut sc = SmoothCount::new(0);
        // snap=true bypasses the +1 tween and jumps to target
        assert_eq!(sc.step(10, 1, true), 10);
        assert_eq!(sc.step(20, 1, true), 20);
    }

    #[test]
    fn smooth_count_advance_one_per_tick() {
        let mut sc = SmoothCount::new(0);
        // First tick at time=10 → advance to 1 (target=5)
        assert_eq!(sc.step(5, 10, false), 1);
        // Same tick: ignored (last_tick == time)
        assert_eq!(sc.step(5, 10, false), 1);
        // New tick: advance again
        assert_eq!(sc.step(5, 11, false), 2);
        assert_eq!(sc.step(5, 12, false), 3);
        assert_eq!(sc.step(5, 13, false), 4);
        assert_eq!(sc.step(5, 14, false), 5);
        // At target: no more advance
        assert_eq!(sc.step(5, 15, false), 5);
    }

    #[test]
    fn smooth_count_target_jump_then_resume() {
        let mut sc = SmoothCount::new(0);
        // Tick to 1
        sc.step(2, 1, false);
        // Now target jumps to 100 — still only +1
        assert_eq!(sc.step(100, 2, false), 2);
        assert_eq!(sc.step(100, 3, false), 3);
    }

    #[test]
    fn remote_progress_review_ready() {
        let input = RemoteSessionInput {
            status: TaskStatus::Completed,
            is_remote_review: true,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::ReviewReady
        );
    }

    #[test]
    fn remote_progress_review_failed() {
        let input = RemoteSessionInput {
            status: TaskStatus::Failed,
            is_remote_review: true,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::ReviewFailed
        );
    }

    #[test]
    fn remote_progress_review_setting_up() {
        let input = RemoteSessionInput {
            status: TaskStatus::Running,
            is_remote_review: true,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::ReviewRunning {
                tail: "setting up".into()
            }
        );
    }

    #[test]
    fn remote_progress_review_with_stage_counts() {
        let input = RemoteSessionInput {
            status: TaskStatus::Running,
            is_remote_review: true,
            todo_completed: 0,
            todo_total: 0,
            review: Some(ReviewProgressInput {
                stage: Some(ReviewStage::Verifying),
                bugs_found: 4,
                bugs_verified: 2,
                bugs_refuted: 1,
            }),
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::ReviewRunning {
                tail: "4 found · 2 verified · 1 refuted".into()
            }
        );
    }

    #[test]
    fn remote_progress_done_pill() {
        let input = RemoteSessionInput {
            status: TaskStatus::Completed,
            is_remote_review: false,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::Done
        );
    }

    #[test]
    fn remote_progress_error_pill() {
        let input = RemoteSessionInput {
            status: TaskStatus::Failed,
            is_remote_review: false,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::Error
        );
    }

    #[test]
    fn remote_progress_status_ellipsis_no_todos() {
        let input = RemoteSessionInput {
            status: TaskStatus::Running,
            is_remote_review: false,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::StatusEllipsis("running…".into())
        );
    }

    #[test]
    fn remote_progress_todo_counts() {
        let input = RemoteSessionInput {
            status: TaskStatus::Running,
            is_remote_review: false,
            todo_completed: 2,
            todo_total: 5,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::TodoCounts {
                completed: 2,
                total: 5
            }
        );
    }

    #[test]
    fn remote_progress_pending_status_ellipsis() {
        let input = RemoteSessionInput {
            status: TaskStatus::Pending,
            is_remote_review: false,
            todo_completed: 0,
            todo_total: 0,
            review: None,
        };
        assert_eq!(
            format_remote_session_progress(&input),
            RemoteProgressLine::StatusEllipsis("pending…".into())
        );
    }
}
