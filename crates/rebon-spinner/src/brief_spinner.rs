//! One-row layout for the brief spinner and for its idle
//! counterpart, as used in `--brief` / assistant mode.
//!
//! The brief spinner is a single-row variant that animates a 1-3 dot cycle
//! next to the verb, with an optional connection-status warning and a
//! right-aligned background-task count.

use crate::shimmer::compute_glimmer_index;
use crate::shimmer_segments::{compute_shimmer_segments, GraphemeWidth, ShimmerSegments};

/// The dot frame index for a monotonic time in milliseconds:
/// `(time / 300) % 3`, so the frame advances every 300ms and cycles
/// through 0, 1, 2.
pub fn brief_dot_frame(time_ms: u64) -> u8 {
    ((time_ms / 300) % 3) as u8
}

/// The dot string for the current frame.
///
/// * `reduced_motion` → returns `"…  "` (one ellipsis + 2 spaces).
/// * Otherwise → one dot per frame plus one (`dot_frame + 1`), then
/// padded with spaces up to 3 columns, so:
/// * frame 0 → `".  "`
/// * frame 1 → `".. "`
/// * frame 2 → `"..."`
pub fn brief_dots(time_ms: u64, reduced_motion: bool) -> String {
    if reduced_motion {
        return "\u{2026}  ".to_string();
    }
    let dot_frame = brief_dot_frame(time_ms);
    let mut s = String::new();
    for _ in 0..=dot_frame {
        s.push('.');
    }
    while s.chars().count() < 3 {
        s.push(' ');
    }
    s
}

/// The full layout decision for one frame of the brief spinner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefSpinnerLayout {
    /// The shimmer segments for the verb. When `show_conn_warning`
    /// is `true`, the consumer renders `conn_text + dots` instead of
    /// these.
    pub segments: ShimmerSegments,
    /// The dot suffix string.
    pub dots: String,
    /// True when the connection is reconnecting / disconnected and
    /// the verb should be replaced with `conn_text`.
    pub show_conn_warning: bool,
    /// The text to show in the conn-warning slot. Empty when no
    /// warning.
    pub conn_text: String,
    /// The right-aligned text ("N in background"). Empty when zero.
    pub right_text: String,
    /// The number of spaces between the left and right text blocks.
    /// Always at least 1.
    pub right_pad: usize,
}

/// Connection status shown by the brief spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    /// Connected and active.
    Active,
    /// Reconnecting — show warning text.
    Reconnecting,
    /// Disconnected — show warning text.
    Disconnected,
}

impl ConnStatus {
    /// Identifies which connection statuses trigger
    /// the warning replacement.
    pub fn shows_warning(self) -> bool {
        matches!(self, ConnStatus::Reconnecting | ConnStatus::Disconnected)
    }

    /// Warning label for the connection status.
    pub fn warning_text(self) -> &'static str {
        match self {
            ConnStatus::Reconnecting => "Reconnecting",
            ConnStatus::Disconnected => "Disconnected",
            ConnStatus::Active => "",
        }
    }
}

/// Compute the visual width of the left half of the spinner row.
/// Equal to either the verb width or the conn-text width, plus 3 (the dots).
pub fn brief_left_width(
    verb_width: usize,
    conn_status: ConnStatus,
    conn_text_width: usize,
) -> usize {
    let base = if conn_status.shows_warning() {
        conn_text_width
    } else {
        verb_width
    };
    base + 3
}

/// Compute the right padding (spaces between left and right text).
/// Always at least 1, plus the leftover space.
pub fn brief_right_pad(columns: usize, left_width: usize, right_text_width: usize) -> usize {
    columns
        .saturating_sub(2)
        .saturating_sub(left_width)
        .saturating_sub(right_text_width)
        .max(1)
}

/// Compute the full brief-spinner layout for one frame.
pub fn brief_spinner_layout<'a>(
    verb_graphemes: &'a [GraphemeWidth<'a>],
    verb_width: usize,
    columns: usize,
    time_ms: u64,
    reduced_motion: bool,
    conn_status: ConnStatus,
    running_count: u64,
) -> BriefSpinnerLayout {
    let dots = brief_dots(time_ms, reduced_motion);
    let show_conn_warning = conn_status.shows_warning();
    let conn_text_str = conn_status.warning_text().to_string();
    let conn_text_width = conn_text_str.chars().count();

    // The glimmer index advances every 150ms, so the tick passed to
    // `compute_glimmer_index` is the elapsed time in 150ms steps.
    let segments = if reduced_motion || show_conn_warning {
        ShimmerSegments {
            before: verb_graphemes
                .iter()
                .map(|g| g.grapheme)
                .collect::<String>(),
            shimmer: String::new(),
            after: String::new(),
        }
    } else {
        let tick = (time_ms / 150) as i64;
        let glimmer_index = compute_glimmer_index(tick, verb_width as i64);
        compute_shimmer_segments(verb_graphemes, verb_width as i64, glimmer_index)
    };

    let right_text = if running_count > 0 {
        format!("{} in background", running_count)
    } else {
        String::new()
    };
    let right_text_width = right_text.chars().count();
    let left_width = brief_left_width(verb_width, conn_status, conn_text_width);
    let right_pad = brief_right_pad(columns, left_width, right_text_width);

    BriefSpinnerLayout {
        segments,
        dots,
        show_conn_warning,
        conn_text: conn_text_str,
        right_text,
        right_pad,
    }
}

/// The layout decision for the idle variant of the brief spinner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefIdleLayout {
    /// True when nothing should render — the blank placeholder of
    /// the same height as the content branch.
    pub is_empty_footprint: bool,
    /// Left text (warning or empty).
    pub left_text: String,
    /// Right text (`"N in background"` or empty).
    pub right_text: String,
    /// Right padding (only meaningful when `!is_empty_footprint`).
    pub right_pad: usize,
}

/// Compute the idle brief-spinner layout: same shape as the brief
/// spinner but with no animation, no verb shimmer.
pub fn brief_idle_layout(
    columns: usize,
    conn_status: ConnStatus,
    running_count: u64,
) -> BriefIdleLayout {
    let left_text = if conn_status.shows_warning() {
        // The idle text is 'Reconnecting…' with an ellipsis, while the
        // animated spinner uses 'Reconnecting' and appends its own dots.
        match conn_status {
            ConnStatus::Reconnecting => "Reconnecting\u{2026}".to_string(),
            ConnStatus::Disconnected => "Disconnected".to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let right_text = if running_count > 0 {
        format!("{} in background", running_count)
    } else {
        String::new()
    };
    if left_text.is_empty() && right_text.is_empty() {
        return BriefIdleLayout {
            is_empty_footprint: true,
            left_text,
            right_text,
            right_pad: 0,
        };
    }
    let right_pad = columns
        .saturating_sub(2)
        .saturating_sub(left_text.chars().count())
        .saturating_sub(right_text.chars().count())
        .max(1);
    BriefIdleLayout {
        is_empty_footprint: false,
        left_text,
        right_text,
        right_pad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_frame_zero() {
        assert_eq!(brief_dot_frame(0), 0);
    }

    #[test]
    fn dot_frame_increments() {
        assert_eq!(brief_dot_frame(300), 1);
        assert_eq!(brief_dot_frame(600), 2);
        assert_eq!(brief_dot_frame(900), 0);
    }

    #[test]
    fn dot_frame_just_under_step() {
        assert_eq!(brief_dot_frame(299), 0);
    }

    #[test]
    fn dots_reduced_motion() {
        assert_eq!(brief_dots(500, true), "\u{2026}  ");
    }

    #[test]
    fn dots_frame_0() {
        assert_eq!(brief_dots(0, false), ".  ");
    }

    #[test]
    fn dots_frame_1() {
        assert_eq!(brief_dots(300, false), ".. ");
    }

    #[test]
    fn dots_frame_2() {
        assert_eq!(brief_dots(600, false), "...");
    }

    #[test]
    fn conn_status_warning_check() {
        assert!(!ConnStatus::Active.shows_warning());
        assert!(ConnStatus::Reconnecting.shows_warning());
        assert!(ConnStatus::Disconnected.shows_warning());
    }

    #[test]
    fn left_width_active_uses_verb() {
        assert_eq!(brief_left_width(10, ConnStatus::Active, 0), 13);
    }

    #[test]
    fn left_width_reconnecting_uses_conn_text() {
        // "Reconnecting" = 12 chars + 3 dots.
        assert_eq!(brief_left_width(5, ConnStatus::Reconnecting, 12), 15);
    }

    #[test]
    fn right_pad_at_least_one() {
        // Tiny terminal — pad clamps to 1.
        assert_eq!(brief_right_pad(10, 50, 50), 1);
    }

    #[test]
    fn right_pad_normal() {
        // 80 - 2 - 20 - 10 = 48 → max with 1 = 48.
        assert_eq!(brief_right_pad(80, 20, 10), 48);
    }

    #[test]
    fn idle_layout_empty_when_nothing_to_show() {
        let l = brief_idle_layout(80, ConnStatus::Active, 0);
        assert!(l.is_empty_footprint);
    }

    #[test]
    fn idle_layout_reconnecting_uses_ellipsis() {
        let l = brief_idle_layout(80, ConnStatus::Reconnecting, 0);
        assert!(!l.is_empty_footprint);
        assert_eq!(l.left_text, "Reconnecting\u{2026}");
    }

    #[test]
    fn idle_layout_disconnected_no_ellipsis() {
        let l = brief_idle_layout(80, ConnStatus::Disconnected, 0);
        assert_eq!(l.left_text, "Disconnected");
    }

    #[test]
    fn idle_layout_running_count() {
        let l = brief_idle_layout(80, ConnStatus::Active, 3);
        assert!(!l.is_empty_footprint);
        assert_eq!(l.right_text, "3 in background");
    }

    #[test]
    fn idle_layout_padding_clamped_to_one() {
        let l = brief_idle_layout(10, ConnStatus::Disconnected, 5);
        assert!(l.right_pad >= 1);
    }

    #[test]
    fn brief_layout_active_renders_verb_segments() {
        let g = vec![
            GraphemeWidth {
                grapheme: "h",
                width: 1,
            },
            GraphemeWidth {
                grapheme: "i",
                width: 1,
            },
        ];
        let l = brief_spinner_layout(&g, 2, 80, 0, false, ConnStatus::Active, 0);
        assert!(!l.show_conn_warning);
        assert_eq!(l.dots, ".  ");
        // Verb width 2; running count 0 → no right text.
        assert_eq!(l.right_text, "");
    }

    #[test]
    fn brief_layout_reconnecting_renders_warning() {
        let g = vec![
            GraphemeWidth {
                grapheme: "h",
                width: 1,
            },
            GraphemeWidth {
                grapheme: "i",
                width: 1,
            },
        ];
        let l = brief_spinner_layout(&g, 2, 80, 0, false, ConnStatus::Reconnecting, 0);
        assert!(l.show_conn_warning);
        assert_eq!(l.conn_text, "Reconnecting");
    }

    #[test]
    fn brief_layout_running_count_in_background() {
        let g = vec![GraphemeWidth {
            grapheme: "x",
            width: 1,
        }];
        let l = brief_spinner_layout(&g, 1, 80, 0, false, ConnStatus::Active, 5);
        assert_eq!(l.right_text, "5 in background");
    }

    #[test]
    fn brief_layout_reduced_motion_no_shimmer_split() {
        let g = vec![
            GraphemeWidth {
                grapheme: "h",
                width: 1,
            },
            GraphemeWidth {
                grapheme: "i",
                width: 1,
            },
        ];
        let l = brief_spinner_layout(&g, 2, 80, 1000, true, ConnStatus::Active, 0);
        assert_eq!(l.dots, "\u{2026}  ");
        assert_eq!(l.segments.before, "hi");
        assert_eq!(l.segments.shimmer, "");
    }
}
