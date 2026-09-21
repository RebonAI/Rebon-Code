//! Background task status pill row.
//!
//! The status row logic is organized around two pure pieces:
//!
//! 1. The pill-row builder: take the running tasks, filter them down
//!    to teammates, prepend the `main` pill, then run the horizontal
//!    scroll window calculation against the available width.
//! 2. The agent-pill projector: pick the right `(label, color)` pair
//!    given `(is_selected, is_viewed, is_idle, is_hover)`.
//!
//! The horizontal-scroll math is kept here as local, dependency-free logic.

use crate::ui::tasks::common::TaskKind;

/// Description of one teammate row used by the pill builder.
#[derive(Debug, Clone)]
pub struct TeammatePillInput {
    /// The teammate task's id.
    pub task_id: String,
    /// The teammate's agent name.
    pub agent_name: String,
    /// Pre-projected teammate color name (e.g. `"blue"`).
    pub agent_color: Option<String>,
    /// True when the teammate is between activities.
    pub is_idle: bool,
    /// Task kind discriminant — used by the runtime caller to filter
    /// out non-teammate tasks before passing them in.
    pub kind: TaskKind,
}

/// Pill row entry — the discriminated form returned by
/// [`build_pill_row`]. The first entry is always the `"main"` pill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PillEntry {
    /// Display name (without the `@` prefix). The `main` pill uses
    /// `"main"`.
    pub name: String,
    /// Pre-projected color name. `None` for `main` and for teammates
    /// without a mapped color.
    pub color: Option<String>,
    /// True when the teammate (or, for `main`, the leader) is idle.
    pub is_idle: bool,
    /// Task id for teammate pills, `None` for the `main` pill.
    pub task_id: Option<String>,
    /// Index in the un-windowed pill list. Stable across scroll
    /// windows so the consumer can match against `selected_index`
    /// even after slicing.
    pub idx: usize,
}

/// Build the full (un-windowed) pill row.
///
/// 1. Sort teammates by agent name. The comparison is byte-wise, which
///    is alphabetical for ASCII names.
/// 2. Map each teammate to a `PillEntry` (color resolved upstream).
/// 3. When `tasks_selected` is false, sort idle teammates to the end
///    (stable within the bucket).
/// 4. Prepend the `main` pill.
/// 5. Assign sequential `idx` values.
pub fn build_pill_row(
    teammates: &[TeammatePillInput],
    is_leader_idle: bool,
    tasks_selected: bool,
) -> Vec<PillEntry> {
    let mut sorted: Vec<&TeammatePillInput> = teammates
        .iter()
        .filter(|t| t.kind == TaskKind::InProcessTeammate)
        .collect();
    sorted.sort_by(|a, b| a.agent_name.cmp(&b.agent_name));

    let mut teammate_pills: Vec<PillEntry> = sorted
        .into_iter()
        .map(|t| PillEntry {
            name: t.agent_name.clone(),
            color: t.agent_color.clone(),
            is_idle: t.is_idle,
            task_id: Some(t.task_id.clone()),
            idx: 0, // assigned below
        })
        .collect();

    if !tasks_selected {
        // Stable sort: idle pills move to the end
        teammate_pills.sort_by(|a, b| match (a.is_idle, b.is_idle) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Equal,
        });
    }

    let mut pills: Vec<PillEntry> = Vec::with_capacity(teammate_pills.len() + 1);
    pills.push(PillEntry {
        name: "main".into(),
        color: None,
        is_idle: is_leader_idle,
        task_id: None,
        idx: 0,
    });
    pills.extend(teammate_pills);

    for (i, p) in pills.iter_mut().enumerate() {
        p.idx = i;
    }
    pills
}

/// Display width of one pill in the row: the rendered `@name` width,
/// plus one for the separator when the pill is not first.
pub fn pill_width(name: &str, idx: usize) -> usize {
    let label = format!("@{name}");
    let base = label.chars().count();
    if idx > 0 {
        base + 1
    } else {
        base
    }
}

/// Result of [`compute_scroll_window`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollWindow {
    /// First visible index, inclusive.
    pub start_index: usize,
    /// Last visible index, exclusive.
    pub end_index: usize,
    /// Show a `←` indicator on the left edge.
    pub show_left_arrow: bool,
    /// Show a `→` indicator on the right edge.
    pub show_right_arrow: bool,
}

/// Horizontal scroll window over a row of widths. The rule is:
///
/// 1. If everything fits, return `[0, len)` and no arrows.
/// 2. Otherwise pick the smallest window that includes the focused
///    element and (when arrows are needed) accounts for the arrow
///    glyph widths (each arrow eats `arrow_padding` columns).
/// 3. Show the left arrow when `start_index > 0` and the right arrow
///    when `end_index < len`.
///
/// The test matrix pins each arrow / boundary case.
pub fn compute_scroll_window(
    widths: &[usize],
    available_width: usize,
    arrow_padding: usize,
    focus_index: usize,
) -> ScrollWindow {
    let len = widths.len();
    if len == 0 {
        return ScrollWindow {
            start_index: 0,
            end_index: 0,
            show_left_arrow: false,
            show_right_arrow: false,
        };
    }
    let total: usize = widths.iter().sum();
    if total <= available_width {
        return ScrollWindow {
            start_index: 0,
            end_index: len,
            show_left_arrow: false,
            show_right_arrow: false,
        };
    }

    // Walk forward from focus_index, then backward, expanding until
    // we run out of width.
    let focus = focus_index.min(len - 1);

    let mut start = focus;
    let mut end = focus + 1;
    // Reserve space for both arrows when we're not yet at the edges.
    let mut budget = available_width.saturating_sub(2 * arrow_padding);
    if widths[focus] > budget {
        // The focus alone is too wide; render just it.
        let show_left_arrow = focus > 0;
        let show_right_arrow = focus + 1 < len;
        return ScrollWindow {
            start_index: focus,
            end_index: focus + 1,
            show_left_arrow,
            show_right_arrow,
        };
    }
    budget -= widths[focus];

    loop {
        let next_right = end < len;
        let next_left = start > 0;
        if !next_left && !next_right {
            break;
        }
        // Prefer extending right
        if next_right {
            let w = widths[end];
            if w <= budget {
                budget -= w;
                end += 1;
                continue;
            }
        }
        if next_left {
            let w = widths[start - 1];
            if w <= budget {
                budget -= w;
                start -= 1;
                continue;
            }
        }
        break;
    }

    let show_left_arrow = start > 0;
    let show_right_arrow = end < len;
    // If a side arrow disappeared (e.g. window includes the first
    // element), credit back its padding to allow one more entry.
    if (!show_left_arrow || !show_right_arrow) && (next_extension(widths, start, end, len) > 0) {
        // Best-effort second pass: try to fit one more entry on the
        // free side.
        let mut extra_budget = 0;
        if !show_left_arrow {
            extra_budget += arrow_padding;
        }
        if !show_right_arrow {
            extra_budget += arrow_padding;
        }
        let mut start = start;
        let mut end = end;
        let mut budget = extra_budget;
        loop {
            let mut advanced = false;
            if end < len && widths[end] <= budget {
                budget -= widths[end];
                end += 1;
                advanced = true;
            }
            if start > 0 && widths[start - 1] <= budget {
                budget -= widths[start - 1];
                start -= 1;
                advanced = true;
            }
            if !advanced {
                break;
            }
        }
        return ScrollWindow {
            start_index: start,
            end_index: end,
            show_left_arrow: start > 0,
            show_right_arrow: end < len,
        };
    }

    ScrollWindow {
        start_index: start,
        end_index: end,
        show_left_arrow,
        show_right_arrow,
    }
}

fn next_extension(_widths: &[usize], start: usize, end: usize, len: usize) -> usize {
    let mut count = 0;
    if start > 0 {
        count += 1;
    }
    if end < len {
        count += 1;
    }
    count
}

/// Pre-built pill style, one variant per branch of [`pill_style`]:
///
/// * `highlighted` (selected or hovered) → inverse background pill
/// * `is_idle` → dim text
/// * `is_viewed` → bold colored text
/// * default → colored text (dimmed when no color)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentPillStyle {
    /// Highlighted (selected/hover) inverse pill.
    HighlightedInverse {
        /// Background color (`None` falls through to `"background"`).
        background_color: Option<String>,
        /// Bold when the pill is the viewed one.
        bold: bool,
    },
    /// Dim teammate (idle).
    DimText {
        /// Bold when the pill is the viewed one.
        bold: bool,
    },
    /// Bold colored text — viewed but not idle, not highlighted.
    BoldColored {
        /// Color name (`None` falls back to default).
        color: Option<String>,
    },
    /// Default colored text (or dim when no color).
    Colored {
        /// Color name. `None` causes the dim default.
        color: Option<String>,
    },
}

/// Picks the style for a given pill given the four state flags.
pub fn pill_style(
    color: Option<&str>,
    is_selected: bool,
    is_viewed: bool,
    is_idle: bool,
    is_hover: bool,
) -> AgentPillStyle {
    let highlighted = is_selected || is_hover;
    if highlighted {
        return AgentPillStyle::HighlightedInverse {
            background_color: color.map(str::to_owned),
            bold: is_viewed,
        };
    }
    if is_idle {
        return AgentPillStyle::DimText { bold: is_viewed };
    }
    if is_viewed {
        return AgentPillStyle::BoldColored {
            color: color.map(str::to_owned),
        };
    }
    AgentPillStyle::Colored {
        color: color.map(str::to_owned),
    }
}

/// Compute the width array for an entire pill row.
pub fn compute_pill_widths(pills: &[PillEntry]) -> Vec<usize> {
    pills
        .iter()
        .enumerate()
        .map(|(i, p)| pill_width(&p.name, i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, idle: bool) -> TeammatePillInput {
        TeammatePillInput {
            task_id: format!("id-{name}"),
            agent_name: name.into(),
            agent_color: Some("blue".into()),
            is_idle: idle,
            kind: TaskKind::InProcessTeammate,
        }
    }

    #[test]
    fn pill_row_main_first_then_teammates_alpha_sorted() {
        let teammates = vec![t("zoe", false), t("alice", false), t("bob", false)];
        let row = build_pill_row(&teammates, false, true);
        let names: Vec<&str> = row.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["main", "alice", "bob", "zoe"]);
        // idx is sequential
        for (i, p) in row.iter().enumerate() {
            assert_eq!(p.idx, i);
        }
        // main has no task_id
        assert!(row[0].task_id.is_none());
        // teammates have ids
        assert_eq!(row[1].task_id.as_deref(), Some("id-alice"));
    }

    #[test]
    fn pill_row_idle_sorted_to_end_when_not_selected() {
        let teammates = vec![t("alice", true), t("bob", false), t("carol", true)];
        let row = build_pill_row(&teammates, false, false);
        // Alpha sort first → alice, bob, carol → then idle to end
        // → bob, alice, carol
        let names: Vec<&str> = row.iter().skip(1).map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["bob", "alice", "carol"]);
    }

    #[test]
    fn pill_row_no_idle_reordering_when_selected() {
        let teammates = vec![t("alice", true), t("bob", false)];
        let row = build_pill_row(&teammates, false, true);
        let names: Vec<&str> = row.iter().skip(1).map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alice", "bob"]);
    }

    #[test]
    fn pill_row_filters_non_teammates() {
        let mut t1 = t("alice", false);
        t1.kind = TaskKind::LocalShell;
        let teammates = vec![t1, t("bob", false)];
        let row = build_pill_row(&teammates, false, true);
        let names: Vec<&str> = row.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["main", "bob"]);
    }

    #[test]
    fn pill_row_main_pill_idle_flag_passes_through() {
        let row = build_pill_row(&[], true, true);
        assert_eq!(row.len(), 1);
        assert!(row[0].is_idle);
    }

    #[test]
    fn pill_width_first_no_separator() {
        // "@main" = 5 chars
        assert_eq!(pill_width("main", 0), 5);
    }

    #[test]
    fn pill_width_subsequent_adds_separator() {
        // "@alice" = 6 chars, +1 separator = 7
        assert_eq!(pill_width("alice", 1), 7);
    }

    #[test]
    fn compute_widths_round_trip() {
        let pills = vec![
            PillEntry {
                name: "main".into(),
                color: None,
                is_idle: false,
                task_id: None,
                idx: 0,
            },
            PillEntry {
                name: "alice".into(),
                color: None,
                is_idle: false,
                task_id: Some("a".into()),
                idx: 1,
            },
        ];
        let widths = compute_pill_widths(&pills);
        assert_eq!(widths, vec![5, 7]);
    }

    #[test]
    fn scroll_window_fits_no_arrows() {
        let widths = vec![5, 7, 8];
        let win = compute_scroll_window(&widths, 100, 2, 0);
        assert_eq!(
            win,
            ScrollWindow {
                start_index: 0,
                end_index: 3,
                show_left_arrow: false,
                show_right_arrow: false,
            }
        );
    }

    #[test]
    fn scroll_window_focus_at_start() {
        // Available width way smaller than the total → window slides
        let widths = vec![5, 5, 5, 5, 5]; // total 25
        let win = compute_scroll_window(&widths, 12, 2, 0);
        // Reserved 4 for arrows, budget 8 for items: focus(5) + maybe 1
        assert_eq!(win.start_index, 0);
        assert!(win.end_index >= 1);
        assert!(!win.show_left_arrow);
        assert!(win.show_right_arrow);
    }

    #[test]
    fn scroll_window_focus_at_end() {
        let widths = vec![5, 5, 5, 5, 5];
        let win = compute_scroll_window(&widths, 12, 2, 4);
        assert_eq!(win.end_index, 5);
        assert!(win.show_left_arrow);
        assert!(!win.show_right_arrow);
    }

    #[test]
    fn scroll_window_focus_in_middle() {
        let widths = vec![5, 5, 5, 5, 5];
        let win = compute_scroll_window(&widths, 12, 2, 2);
        assert!(win.show_left_arrow);
        assert!(win.show_right_arrow);
        assert!(win.start_index <= 2);
        assert!(win.end_index > 2);
    }

    #[test]
    fn scroll_window_empty_input() {
        let win = compute_scroll_window(&[], 100, 2, 0);
        assert_eq!(win.start_index, 0);
        assert_eq!(win.end_index, 0);
    }

    #[test]
    fn pill_style_highlighted() {
        let s = pill_style(Some("blue"), true, false, false, false);
        assert_eq!(
            s,
            AgentPillStyle::HighlightedInverse {
                background_color: Some("blue".into()),
                bold: false,
            }
        );
    }

    #[test]
    fn pill_style_hover_acts_as_highlighted() {
        let s = pill_style(None, false, false, false, true);
        assert!(matches!(s, AgentPillStyle::HighlightedInverse { .. }));
    }

    #[test]
    fn pill_style_idle_dim() {
        let s = pill_style(Some("blue"), false, false, true, false);
        assert_eq!(s, AgentPillStyle::DimText { bold: false });
    }

    #[test]
    fn pill_style_idle_dim_bold_when_viewed() {
        let s = pill_style(Some("blue"), false, true, true, false);
        assert_eq!(s, AgentPillStyle::DimText { bold: true });
    }

    #[test]
    fn pill_style_viewed_bold_colored() {
        let s = pill_style(Some("green"), false, true, false, false);
        assert_eq!(
            s,
            AgentPillStyle::BoldColored {
                color: Some("green".into())
            }
        );
    }

    #[test]
    fn pill_style_default_colored_no_color_dim() {
        let s = pill_style(None, false, false, false, false);
        assert_eq!(s, AgentPillStyle::Colored { color: None });
    }
}
