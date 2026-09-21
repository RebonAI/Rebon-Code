//! # rebon-spinner — state-machine helpers for spinner rows
//!
//! Pure loading-spinner state and projection helpers for the row(s) below the
//! chat transcript while the assistant, a tool, or a teammate is working.
//!
//! ## Behavior modules
//!
//! * [`glyph`] chooses the animated glyph sequence, the reduced-motion dot, the
//! platform-specific characters, and the stalled-color interpolation.
//! * [`color`], [`flashing`] and [`thinking_shimmer`] handle RGB
//! interpolation, `rgb(r,g,b)` formatting, hue conversion, tool-use flash
//! colors, and the thinking glow.
//! * [`shimmer`], [`shimmer_char`] and [`shimmer_segments`] compute sweep
//! timing, highlighted cells, and grapheme-aware before / shimmer / after
//! message segments.
//! * [`stalled`] tracks the no-token timeout and the smooth red-fade intensity
//! for stalled rows.
//! * [`row_layout`], [`elapsed`], [`token_counter`] and [`thinking_status`]
//! decide which status pieces fit in the spinner row, including timer, token
//! count, effort suffixes, pause accounting, and minimum display timing.
//! * [`teammate_line`] and [`teammate_tree`] build leader and teammate row
//! layouts with tree characters, name truncation, stats and hint gating,
//! selection highlights, and hide-row footer decisions.
//! * [`task_planner`] selects the next unblocked pending task.
//! * [`brief_spinner`] and [`spinner_branch`] choose between the brief, idle,
//! leader, teammate and regular spinner branches.
//! * [`glimmer_message`] chooses the empty, stalled, tool-use, plain or shimmer
//! message branch.
//!
//! ## Caller-owned inputs
//!
//! The seams into the surrounding app are modeled as plain data, never as trait
//! objects:
//!
//! 1. **Theme palette** — theme keys are resolved outside this crate and passed
//!    in as `Option<RgbColor>`; [`color::parse_rgb`] turns a palette's
//!    `"rgb(r,g,b)"` string into one. The crate never implements a theme.
//! 2. **Teammate task state** — modeled as the owned
//!    [`teammate_line::TeammateLineInputs`] struct, carrying only the fields
//!    read when laying out a teammate row.
//! 3. **Everything else** — activity tracking, app and task state, spinner
//!    verbs, effort suffixes, turn output tokens, global config, grapheme
//!    segmentation, display width, elapsed time, terminal size, animation
//!    frames, feature and settings gates, and recent-activity summaries.
//!    Consumers pass plain owned structs.
//!
//! Deliberately not done here:
//!
//! * Drawing. Every module returns a plain decision struct or string; the
//! consumer draws it however it likes.
//! * Grapheme segmentation and display-width measurement. Both are
//! caller-owned: [`shimmer_segments`] takes a slice of pre-segmented
//! `(grapheme, width)` pairs.
//! * The animation-frame driver. The crate is purely tick-driven: the consumer
//! feeds an absolute monotonic `time_ms: u64` per frame.
//! * ANSI color emission. Modules return semantic color enums or `RgbColor`
//! values.
//!
//! These are omitted rather than carried as stubs: a placeholder that hints at
//! the wrong API is harder to correct downstream than an honest absence.
//!
//! `rebon-types` is the only `rebon-*` crate this one depends on, and only for
//! the task-list row the next-task planner reads — defined once there rather
//! than copied into every surface that reads it.

#![deny(missing_docs)]

pub mod brief_spinner;
pub mod color;
pub mod elapsed;
pub mod flashing;
pub mod glimmer_message;
pub mod glyph;
pub mod row_layout;
pub mod shimmer;
pub mod shimmer_char;
pub mod shimmer_segments;
pub mod spinner_branch;
pub mod stalled;
pub mod task_planner;
pub mod teammate_line;
pub mod teammate_tree;
pub mod thinking_shimmer;
pub mod thinking_status;
pub mod token_counter;

pub use brief_spinner::{
    brief_dot_frame, brief_dots, brief_left_width, brief_right_pad, BriefIdleLayout,
    BriefSpinnerLayout,
};
pub use color::{hue_to_rgb, interpolate_color, parse_rgb, to_rgb_string, RgbColor};
pub use elapsed::{
    current_elapsed_snapshot, effective_elapsed_ms, should_show_btw_tip, should_show_clear_tip,
    BTW_TIP_THRESHOLD_MS, CLEAR_TIP_THRESHOLD_MS,
};
pub use flashing::{flash_opacity_at, flashing_char_color, FlashingResult};
pub use glimmer_message::{glimmer_message_branch, GlimmerBranch, GlimmerColor};
pub use glyph::{
    default_characters, glyph_for_frame, reduced_motion_dot_is_dim, spinner_frames,
    stalled_glyph_color, tool_call_spinner_frame, tool_call_spinner_glyph, GlyphPlatform,
    StalledColor, ERROR_RED, REDUCED_MOTION_CYCLE_MS, REDUCED_MOTION_DOT,
};
pub use row_layout::{progressive_status_layout, RowLayout, THINKING_BARE_WIDTH};
pub use shimmer::{
    compute_glimmer_index, compute_glimmer_index_for_mode, glimmer_speed_for_mode, SpinnerMode,
    REQUESTING_GLIMMER_SPEED_MS, STALLED_GLIMMER_INDEX, TOOL_USE_GLIMMER_SPEED_MS,
};
pub use shimmer_char::should_use_shimmer;
pub use shimmer_segments::{
    compute_shimmer_segments, glimmer_message_segments, GraphemeWidth, ShimmerSegments,
};
pub use spinner_branch::{spinner_branch, SpinnerBranch, SpinnerBranchInputs};
pub use stalled::{StalledAnimation, StalledTick, STALLED_FADE_DURATION_MS, STALLED_THRESHOLD_MS};
pub use task_planner::{find_next_pending_task, ListTask, TaskListStatus};
pub use teammate_line::{
    teammate_line_layout, TeammateInfo, TeammateLineLayout, TeammateProgress, TeammateStatusKind,
    BASE_PREFIX_WIDTH, MIN_ACTIVITY_WIDTH, MIN_FULL_NAME_COLUMNS,
};
pub use teammate_tree::{teammate_tree_layout, LeaderRow, TeammateTreeLayout, TreeChars};
pub use thinking_shimmer::{
    thinking_shimmer_color, THINKING_DELAY_MS, THINKING_GLOW_PERIOD_S, THINKING_INACTIVE,
    THINKING_INACTIVE_SHIMMER,
};
pub use thinking_status::{ThinkingStatus, ThinkingStatusEvent, ThinkingStatusReducer};
pub use token_counter::tween_token_counter;

/// Keyboard hint shown when teammate selection is available.
pub const TEAMMATE_SELECT_HINT: &str = "Shift + ↑/↓ to select";

/// The teardrop-asterisk glyph used for the idle status text, kept here
/// as a `&str` so the crate compiles standalone.
pub const TEARDROP_ASTERISK: &str = "✻";

#[cfg(test)]
mod compatibility {
    /// Compatibility canary — `rebon-types` is the only `rebon-*` crate
    /// `rebon-spinner` may depend on, and only because the next-task planner
    /// reads task-list rows, which are defined there once for every surface
    /// that reads them. Adding any other `rebon-*` dep means updating the
    /// crate-level docstring and this list, deliberately.
    const ALLOWED_REBON_DEPS: &[&str] = &["rebon-types"];

    #[test]
    fn only_allowed_rebon_deps_in_cargo_toml() {
        let cargo = include_str!("../Cargo.toml");
        for line in cargo.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with("rebon-") {
                let dep_name = trimmed.split('=').next().unwrap_or("").trim();
                assert!(
                    ALLOWED_REBON_DEPS.contains(&dep_name),
                    "unexpected rebon dep in rebon-spinner; found: {line}"
                );
            }
        }
    }

    #[test]
    fn teammate_select_hint_pinned() {
        assert_eq!(
            super::TEAMMATE_SELECT_HINT,
            "Shift + \u{2191}/\u{2193} to select"
        );
    }
}
