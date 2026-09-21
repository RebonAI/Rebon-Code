//! Four-step session-teleport progress list.
//!
//! Describes the four-step progress list shown while teleporting a
//! remote session, with the current step animated and completed steps
//! marked with a tick.
//!
//! Pure logic covered here:
//!
//! 1. The four-step list ([`TELEPORT_STEPS`]).
//! 2. The current/complete/pending status dispatch
//!    ([`teleport_progress_layout`]).
//! 3. The spinner-frame index ([`SPINNER_FRAMES`]).
//! 4. The icon + color picking for each row.
//!
//! The animation clock and the tick/circle glyphs are pinned constants
//! plus a frame-index input.

/// The four progress steps in order.
pub const TELEPORT_STEPS: &[(TeleportProgressStep, &str)] = &[
    (TeleportProgressStep::Validating, "Validating session"),
    (TeleportProgressStep::FetchingLogs, "Fetching session logs"),
    (TeleportProgressStep::FetchingBranch, "Getting branch info"),
    (TeleportProgressStep::CheckingOut, "Checking out branch"),
];

/// Spinner frames, cycled one per 100ms of animation time.
pub const SPINNER_FRAMES: &[&str] = &["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"];

/// Tick glyph marking a completed step: `'✔'` (U+2714).
pub const TICK_GLYPH: &str = "\u{2714}";

/// Circle glyph marking a step that has not started: `'◯'` (U+25EF).
pub const CIRCLE_GLYPH: &str = "\u{25ef}";

/// One progress step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TeleportProgressStep {
    /// `'validating'` — first step, validates the session.
    Validating,
    /// `'fetching_logs'` — fetches session logs.
    FetchingLogs,
    /// `'fetching_branch'` — fetches branch info.
    FetchingBranch,
    /// `'checking_out'` — checks out the branch.
    CheckingOut,
}

/// Status of a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeleportStepStatus {
    /// Step is already finished.
    Complete,
    /// Step is in progress.
    Current,
    /// Step has not started.
    Pending,
}

/// One row in the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeleportStepRow {
    /// The step this row is for.
    pub step: TeleportProgressStep,
    /// Human-readable label.
    pub label: &'static str,
    /// Step status.
    pub status: TeleportStepStatus,
    /// Glyph (`tick`, spinner frame, or `circle`).
    pub icon: &'static str,
    /// Optional color theme key.
    /// `Some("green")`, `Some("rebon")`, or `None` (pending).
    pub color: Option<&'static str>,
}

/// Top-level layout of the progress list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeleportProgressLayout {
    /// Header text. Pinned: `"Teleporting session…"` with the leading
    /// spinner glyph.
    pub header: String,
    /// Optional session id sub-header.
    pub session_id: Option<String>,
    /// Step rows in order.
    pub rows: Vec<TeleportStepRow>,
}

/// Index into [`SPINNER_FRAMES`] for the given absolute monotonic
/// time in milliseconds: one frame per 100ms, wrapping at the frame
/// count.
pub fn spinner_frame_at(time_ms: u64) -> usize {
    ((time_ms / 100) as usize) % SPINNER_FRAMES.len()
}

/// Build the progress layout for the given step and animation time.
pub fn teleport_progress_layout(
    current_step: TeleportProgressStep,
    session_id: Option<&str>,
    time_ms: u64,
) -> TeleportProgressLayout {
    let frame = spinner_frame_at(time_ms);
    let spinner = SPINNER_FRAMES[frame];

    let current_idx = TELEPORT_STEPS
        .iter()
        .position(|(s, _)| *s == current_step)
        .unwrap_or(0);

    let mut rows = Vec::with_capacity(TELEPORT_STEPS.len());
    for (idx, (step, label)) in TELEPORT_STEPS.iter().enumerate() {
        let (status, icon, color) = if idx < current_idx {
            (TeleportStepStatus::Complete, TICK_GLYPH, Some("green"))
        } else if idx == current_idx {
            (TeleportStepStatus::Current, spinner, Some("rebon"))
        } else {
            (TeleportStepStatus::Pending, CIRCLE_GLYPH, None)
        };
        rows.push(TeleportStepRow {
            step: *step,
            label,
            status,
            icon,
            color,
        });
    }

    TeleportProgressLayout {
        header: format!("{spinner} Teleporting session\u{2026}"),
        session_id: session_id.map(Into::into),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_count_is_four() {
        assert_eq!(TELEPORT_STEPS.len(), 4);
    }

    #[test]
    fn step_labels_are_expected() {
        assert_eq!(TELEPORT_STEPS[0].1, "Validating session");
        assert_eq!(TELEPORT_STEPS[1].1, "Fetching session logs");
        assert_eq!(TELEPORT_STEPS[2].1, "Getting branch info");
        assert_eq!(TELEPORT_STEPS[3].1, "Checking out branch");
    }

    #[test]
    fn spinner_frames_count_is_four() {
        assert_eq!(SPINNER_FRAMES.len(), 4);
    }

    #[test]
    fn spinner_frame_index_at_zero_is_zero() {
        assert_eq!(spinner_frame_at(0), 0);
    }

    #[test]
    fn spinner_frame_index_at_100ms_is_one() {
        assert_eq!(spinner_frame_at(100), 1);
    }

    #[test]
    fn spinner_frame_index_wraps_after_four_frames() {
        assert_eq!(spinner_frame_at(400), 0);
    }

    #[test]
    fn spinner_frame_within_100ms_is_constant() {
        assert_eq!(spinner_frame_at(50), 0);
        assert_eq!(spinner_frame_at(99), 0);
    }

    #[test]
    fn validating_makes_first_row_current_others_pending() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, None, 0);
        assert_eq!(layout.rows[0].status, TeleportStepStatus::Current);
        assert_eq!(layout.rows[1].status, TeleportStepStatus::Pending);
        assert_eq!(layout.rows[2].status, TeleportStepStatus::Pending);
        assert_eq!(layout.rows[3].status, TeleportStepStatus::Pending);
    }

    #[test]
    fn fetching_logs_marks_first_complete_second_current() {
        let layout = teleport_progress_layout(TeleportProgressStep::FetchingLogs, None, 0);
        assert_eq!(layout.rows[0].status, TeleportStepStatus::Complete);
        assert_eq!(layout.rows[1].status, TeleportStepStatus::Current);
        assert_eq!(layout.rows[2].status, TeleportStepStatus::Pending);
        assert_eq!(layout.rows[3].status, TeleportStepStatus::Pending);
    }

    #[test]
    fn checking_out_marks_three_complete_one_current() {
        let layout = teleport_progress_layout(TeleportProgressStep::CheckingOut, None, 0);
        for (i, row) in layout.rows.iter().enumerate() {
            if i == 3 {
                assert_eq!(row.status, TeleportStepStatus::Current);
            } else {
                assert_eq!(row.status, TeleportStepStatus::Complete);
            }
        }
    }

    #[test]
    fn complete_rows_use_tick_glyph() {
        let layout = teleport_progress_layout(TeleportProgressStep::CheckingOut, None, 0);
        assert_eq!(layout.rows[0].icon, TICK_GLYPH);
        assert_eq!(layout.rows[1].icon, TICK_GLYPH);
        assert_eq!(layout.rows[2].icon, TICK_GLYPH);
    }

    #[test]
    fn complete_rows_use_green_color() {
        let layout = teleport_progress_layout(TeleportProgressStep::CheckingOut, None, 0);
        assert_eq!(layout.rows[0].color, Some("green"));
    }

    #[test]
    fn pending_rows_use_circle_glyph_and_no_color() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, None, 0);
        assert_eq!(layout.rows[1].icon, CIRCLE_GLYPH);
        assert_eq!(layout.rows[1].color, None);
    }

    #[test]
    fn current_row_uses_spinner_frame() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, None, 100);
        // 100ms → frame 1 → "◓"
        assert_eq!(layout.rows[0].icon, "\u{25d3}");
    }

    #[test]
    fn current_row_uses_claude_color() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, None, 0);
        assert_eq!(layout.rows[0].color, Some("rebon"));
    }

    #[test]
    fn header_contains_spinner_and_text() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, None, 0);
        assert!(layout.header.starts_with("\u{25d0}"));
        assert!(layout.header.contains("Teleporting session"));
        assert!(layout.header.ends_with('\u{2026}'));
    }

    #[test]
    fn session_id_is_optional_and_passed_through() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, Some("abc-123"), 0);
        assert_eq!(layout.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn no_session_id_when_none() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, None, 0);
        assert_eq!(layout.session_id, None);
    }

    #[test]
    fn rows_preserve_step_identity() {
        let layout = teleport_progress_layout(TeleportProgressStep::Validating, None, 0);
        assert_eq!(layout.rows[0].step, TeleportProgressStep::Validating);
        assert_eq!(layout.rows[1].step, TeleportProgressStep::FetchingLogs);
        assert_eq!(layout.rows[2].step, TeleportProgressStep::FetchingBranch);
        assert_eq!(layout.rows[3].step, TeleportProgressStep::CheckingOut);
    }
}
