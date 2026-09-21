//! Tag-tabs windowing reducer.
//!
//! This module computes:
//!
//! 1. A resume-label text (`"Resume"` or `"Resume (All Projects)"`).
//! 2. A max tabs region width: `available_width - resume_label_width
//!    - 1 - max(rhint_with_count, rhint_no_count) - 2`.
//! 3. Per-tab widths (with optional truncation to
//!    `max(20, floor(max_tabs_width/2))`).
//! 4. A visible window [start, end) centered on the
//!    (clamped) selected index: grows alternately left / right while
//!    the next candidate fits.
//! 5. `hidden_left` is the number of tabs before the window and
//!    `hidden_right` the number after it.
//! 6. Per-visible-tab display text (`All` as-is, otherwise
//!    `#{truncated_tag}`).
//!
//! Both display-width computations (display columns and
//! truncate-to-width) do not commit to any specific unicode-width
//! strategy; display-width is taken as an injected function so callers
//! can wire `unicode-width` / `unicode-segmentation` without forcing
//! a dep on this crate. Tests use a simple `char-count` width so the
//! reducer logic itself is pinned independently of the chosen strategy.

/// Layout constants for the tag-tabs chrome.
pub const ALL_TAB_LABEL: &str = "All";
pub const TAB_PADDING: usize = 2;
pub const HASH_PREFIX_LENGTH: usize = 1;
pub const LEFT_ARROW_PREFIX: &str = "\u{2190} ";
pub const RIGHT_HINT_WITH_COUNT_PREFIX: &str = "\u{2192}";
pub const RIGHT_HINT_SUFFIX: &str = " (tab to cycle)";
pub const RIGHT_HINT_NO_COUNT: &str = "(tab to cycle)";
pub const MAX_OVERFLOW_DIGITS: usize = 2;

/// `"← NN "` computed width: `LEFT_ARROW_PREFIX.len() + MAX_OVERFLOW_DIGITS + 1`.
/// `"← "` counts as 2 characters (the arrow glyph is a single code
/// unit); we fix the character count at 2 rather than measuring at
/// runtime.
pub const LEFT_ARROW_WIDTH: usize = 2 + MAX_OVERFLOW_DIGITS + 1;
/// `"→NN (tab to cycle)"` width. The arrow glyph is 1 code unit; we pin
/// the character count.
pub const RIGHT_HINT_WIDTH_WITH_COUNT: usize = 1 + MAX_OVERFLOW_DIGITS + RIGHT_HINT_SUFFIX.len();
/// `"(tab to cycle)"` width.
pub const RIGHT_HINT_WIDTH_NO_COUNT: usize = RIGHT_HINT_NO_COUNT.len();

/// The resume-label string: `"Resume (All Projects)"` when
/// `show_all_projects` is set, plain `"Resume"` otherwise.
pub fn resume_label(show_all_projects: bool) -> &'static str {
    if show_all_projects {
        "Resume (All Projects)"
    } else {
        "Resume"
    }
}

/// Display-width callback. Tests use `|s| s.chars().count()`. Consumers
/// plug in `unicode-width` or similar.
pub type StringWidthFn<'a> = &'a dyn Fn(&str) -> usize;

/// Truncate callback —
/// returns a display-width-capped prefix of the tag, possibly with
/// an ellipsis. We take it as a callback so this crate doesn't force
/// an ellipsis strategy.
pub type TruncateToWidthFn<'a> = &'a dyn Fn(&str, usize) -> String;

/// Inputs for the tag-tabs projection.
#[derive(Debug, Clone)]
pub struct TagTabsInput {
    pub tabs: Vec<String>,
    pub selected_index: i64,
    pub available_width: usize,
    pub show_all_projects: bool,
}

/// A single visible tab, with its actual index and display flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleTab {
    pub actual_index: usize,
    pub display_text: String,
    pub is_selected: bool,
}

/// The projected layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagTabsLayout {
    pub resume_label: &'static str,
    /// Clamped selected index. `0` when tabs is empty.
    pub safe_selected_index: usize,
    /// Number of tabs hidden to the left of the visible window.
    pub hidden_left: usize,
    /// Number of tabs hidden to the right of the visible window.
    pub hidden_right: usize,
    pub visible_tabs: Vec<VisibleTab>,
    /// Exposed for introspection / debugging.
    pub max_tabs_width: usize,
    /// Exposed for introspection / debugging.
    pub max_single_tab_width: usize,
}

/// Calculate the display width of a tab.
pub fn get_tab_width(
    tab: &str,
    max_width: Option<usize>,
    string_width: StringWidthFn<'_>,
) -> usize {
    if tab == ALL_TAB_LABEL {
        return ALL_TAB_LABEL.len() + TAB_PADDING;
    }
    let tag_width = string_width(tab);
    let effective = match max_width {
        Some(m) => {
            let limit = m.saturating_sub(TAB_PADDING + HASH_PREFIX_LENGTH);
            tag_width.min(limit)
        }
        None => tag_width,
    };
    effective + TAB_PADDING + HASH_PREFIX_LENGTH
}

/// Truncate a tag to fit within `max_width`.
pub fn truncate_tag(
    tag: &str,
    max_width: usize,
    string_width: StringWidthFn<'_>,
    truncate: TruncateToWidthFn<'_>,
) -> String {
    let available = max_width.saturating_sub(TAB_PADDING + HASH_PREFIX_LENGTH);
    if string_width(tag) <= available {
        return tag.to_string();
    }
    if available <= 1 {
        return tag
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default();
    }
    truncate(tag, available)
}

/// Full layout projection. Pure: no state, no side effects.
pub fn project_tag_tabs(
    input: &TagTabsInput,
    string_width: StringWidthFn<'_>,
    truncate: TruncateToWidthFn<'_>,
) -> TagTabsLayout {
    let resume = resume_label(input.show_all_projects);
    let resume_width = resume.len() + 1; // +1 gap
    let right_hint_width = RIGHT_HINT_WIDTH_WITH_COUNT.max(RIGHT_HINT_WIDTH_NO_COUNT);
    let max_tabs_width = input
        .available_width
        .saturating_sub(resume_width + right_hint_width + 2);

    // Clamp selected index. When tabs is empty, stay at 0.
    let safe_selected_index = if input.tabs.is_empty() {
        0
    } else {
        let last = (input.tabs.len() - 1) as i64;
        input.selected_index.clamp(0, last) as usize
    };

    // Per-tab widths, with per-tab truncation to
    // max(20, floor(max_tabs_width / 2)).
    let max_single_tab_width = 20.max(max_tabs_width / 2);
    let tab_widths: Vec<usize> = input
        .tabs
        .iter()
        .map(|t| get_tab_width(t, Some(max_single_tab_width), string_width))
        .collect();

    let total_tabs_width: usize = tab_widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            w + if i < tab_widths.len().saturating_sub(1) {
                1
            } else {
                0
            }
        })
        .sum();

    let (start_index, end_index) = if total_tabs_width > max_tabs_width {
        // Window from selected_index outward, left/right alternating.
        let effective_max = max_tabs_width.saturating_sub(LEFT_ARROW_WIDTH);
        let mut start = safe_selected_index;
        let mut end = safe_selected_index + 1;
        let mut window_width = tab_widths.get(safe_selected_index).copied().unwrap_or(0);
        loop {
            let can_left = start > 0;
            let can_right = end < input.tabs.len();
            if !can_left && !can_right {
                break;
            }
            if can_left {
                let left_w = tab_widths.get(start - 1).copied().unwrap_or(0) + 1;
                if window_width + left_w <= effective_max {
                    start -= 1;
                    window_width += left_w;
                    continue;
                }
            }
            if can_right {
                let right_w = tab_widths.get(end).copied().unwrap_or(0) + 1;
                if window_width + right_w <= effective_max {
                    end += 1;
                    window_width += right_w;
                    continue;
                }
            }
            break;
        }
        (start, end)
    } else {
        (0, input.tabs.len())
    };

    let hidden_left = start_index;
    let hidden_right = input.tabs.len().saturating_sub(end_index);

    let visible_tabs: Vec<VisibleTab> = input.tabs[start_index..end_index]
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let actual_index = start_index + i;
            let is_selected = actual_index == safe_selected_index;
            let display_text = if tab == ALL_TAB_LABEL {
                tab.clone()
            } else {
                let truncated = truncate_tag(
                    tab,
                    max_single_tab_width.saturating_sub(TAB_PADDING),
                    string_width,
                    truncate,
                );
                format!("#{truncated}")
            };
            VisibleTab {
                actual_index,
                display_text,
                is_selected,
            }
        })
        .collect();

    TagTabsLayout {
        resume_label: resume,
        safe_selected_index,
        hidden_left,
        hidden_right,
        visible_tabs,
        max_tabs_width,
        max_single_tab_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sw(s: &str) -> usize {
        s.chars().count()
    }
    fn trunc(s: &str, w: usize) -> String {
        s.chars().take(w).collect()
    }

    fn call(input: &TagTabsInput) -> TagTabsLayout {
        project_tag_tabs(input, &sw, &trunc)
    }

    fn tabs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // ---- resume label ----

    #[test]
    fn resume_label_default() {
        assert_eq!(resume_label(false), "Resume");
    }

    #[test]
    fn resume_label_all_projects() {
        assert_eq!(resume_label(true), "Resume (All Projects)");
    }

    // ---- get_tab_width ----

    #[test]
    fn get_tab_width_all_is_label_plus_padding() {
        assert_eq!(get_tab_width("All", None, &sw), 3 + 2);
    }

    #[test]
    fn get_tab_width_tag_adds_hash_and_padding() {
        // "bug" → 3 + 2 + 1 = 6
        assert_eq!(get_tab_width("bug", None, &sw), 6);
    }

    #[test]
    fn get_tab_width_caps_to_max_width() {
        // max_width=10 → limit for tag = 10 - 2 - 1 = 7. Tag "aaaaaaaaaa"
        // (10 chars) caps to 7; returned = 7 + 2 + 1 = 10.
        assert_eq!(get_tab_width("aaaaaaaaaa", Some(10), &sw), 10);
    }

    #[test]
    fn get_tab_width_tag_shorter_than_cap() {
        assert_eq!(get_tab_width("short", Some(100), &sw), 5 + 2 + 1);
    }

    // ---- truncate_tag ----

    #[test]
    fn truncate_tag_no_truncation_when_fits() {
        // available = 10 - 2 - 1 = 7; "abcd" fits.
        assert_eq!(truncate_tag("abcd", 10, &sw, &trunc), "abcd");
    }

    #[test]
    fn truncate_tag_single_char_fallback() {
        // available = 3 - 2 - 1 = 0 → hit the `<= 1` branch → first char only.
        assert_eq!(truncate_tag("abcd", 3, &sw, &trunc), "a");
    }

    #[test]
    fn truncate_tag_uses_truncate_callback() {
        // available = 6 - 2 - 1 = 3; "abcdef" truncates to 3.
        assert_eq!(truncate_tag("abcdef", 6, &sw, &trunc), "abc");
    }

    #[test]
    fn truncate_tag_empty_safe() {
        assert_eq!(truncate_tag("", 10, &sw, &trunc), "");
    }

    // ---- project_tag_tabs: visible window ----

    #[test]
    fn empty_tabs_returns_empty_layout() {
        let l = call(&TagTabsInput {
            tabs: vec![],
            selected_index: 0,
            available_width: 200,
            show_all_projects: false,
        });
        assert_eq!(l.visible_tabs.len(), 0);
        assert_eq!(l.hidden_left, 0);
        assert_eq!(l.hidden_right, 0);
    }

    #[test]
    fn all_tabs_fit_no_hidden() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["All", "bug", "feat"]),
            selected_index: 0,
            available_width: 200,
            show_all_projects: false,
        });
        assert_eq!(l.hidden_left, 0);
        assert_eq!(l.hidden_right, 0);
        assert_eq!(l.visible_tabs.len(), 3);
        assert_eq!(l.visible_tabs[0].display_text, "All");
        assert_eq!(l.visible_tabs[1].display_text, "#bug");
        assert_eq!(l.visible_tabs[2].display_text, "#feat");
    }

    #[test]
    fn selected_flag_marks_selection() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["All", "bug", "feat"]),
            selected_index: 1,
            available_width: 200,
            show_all_projects: false,
        });
        assert!(!l.visible_tabs[0].is_selected);
        assert!(l.visible_tabs[1].is_selected);
        assert!(!l.visible_tabs[2].is_selected);
    }

    #[test]
    fn selected_index_clamped_to_last_when_too_high() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["a", "b", "c"]),
            selected_index: 999,
            available_width: 200,
            show_all_projects: false,
        });
        assert_eq!(l.safe_selected_index, 2);
    }

    #[test]
    fn selected_index_clamped_to_zero_when_negative() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["a", "b", "c"]),
            selected_index: -5,
            available_width: 200,
            show_all_projects: false,
        });
        assert_eq!(l.safe_selected_index, 0);
    }

    #[test]
    fn narrow_terminal_hides_some_right() {
        // Many tabs, narrow terminal.
        let l = call(&TagTabsInput {
            tabs: tabs(&["All", "aa", "bb", "cc", "dd", "ee", "ff", "gg"]),
            selected_index: 0,
            available_width: 50,
            show_all_projects: false,
        });
        assert!(l.hidden_right > 0);
        assert_eq!(l.hidden_left, 0);
        assert!(l.visible_tabs.iter().any(|v| v.is_selected));
    }

    #[test]
    fn selection_near_end_hides_left() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["aa", "bb", "cc", "dd", "ee", "ff", "gg", "hh"]),
            selected_index: 7,
            available_width: 40,
            show_all_projects: false,
        });
        assert!(l.hidden_left > 0);
        assert!(l.visible_tabs.last().unwrap().is_selected);
    }

    #[test]
    fn selection_in_middle_is_visible() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9"]),
            selected_index: 5,
            available_width: 45,
            show_all_projects: false,
        });
        assert!(l
            .visible_tabs
            .iter()
            .any(|v| v.is_selected && v.actual_index == 5));
    }

    #[test]
    fn long_tag_is_truncated_with_hash_prefix() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["All", "averylongtagthatgoesonandonforever"]),
            selected_index: 1,
            available_width: 40,
            show_all_projects: false,
        });
        let tag = l.visible_tabs.iter().find(|v| v.actual_index == 1).unwrap();
        assert!(tag.display_text.starts_with('#'));
        assert!(tag.display_text.len() < "averylongtagthatgoesonandonforever".len() + 1);
    }

    #[test]
    fn all_tab_renders_without_hash() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["All", "x"]),
            selected_index: 0,
            available_width: 200,
            show_all_projects: false,
        });
        assert_eq!(l.visible_tabs[0].display_text, "All");
    }

    #[test]
    fn hidden_counts_sum_to_total_minus_visible() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7"]),
            selected_index: 4,
            available_width: 35,
            show_all_projects: false,
        });
        assert_eq!(l.hidden_left + l.hidden_right + l.visible_tabs.len(), 8);
    }

    #[test]
    fn visible_tabs_preserve_order() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["All", "bug", "feat", "docs"]),
            selected_index: 1,
            available_width: 200,
            show_all_projects: false,
        });
        let indices: Vec<usize> = l.visible_tabs.iter().map(|v| v.actual_index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn width_constants_pinned() {
        assert_eq!(ALL_TAB_LABEL, "All");
        assert_eq!(TAB_PADDING, 2);
        assert_eq!(HASH_PREFIX_LENGTH, 1);
        assert_eq!(LEFT_ARROW_PREFIX, "\u{2190} ");
        assert_eq!(RIGHT_HINT_SUFFIX, " (tab to cycle)");
        assert_eq!(RIGHT_HINT_NO_COUNT, "(tab to cycle)");
        assert_eq!(MAX_OVERFLOW_DIGITS, 2);
    }

    #[test]
    fn max_tabs_width_saturates_to_zero_on_narrow() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["a"]),
            selected_index: 0,
            available_width: 5,
            show_all_projects: false,
        });
        assert_eq!(l.max_tabs_width, 0);
    }

    #[test]
    fn resume_all_projects_increases_label() {
        let l = call(&TagTabsInput {
            tabs: tabs(&["a"]),
            selected_index: 0,
            available_width: 200,
            show_all_projects: true,
        });
        assert_eq!(l.resume_label, "Resume (All Projects)");
    }

    #[test]
    fn zero_width_max_single_tab_floor_is_twenty() {
        // max_tabs_width=0 → max_single_tab_width = max(20, 0) = 20
        let l = call(&TagTabsInput {
            tabs: tabs(&["x"]),
            selected_index: 0,
            available_width: 5,
            show_all_projects: false,
        });
        assert_eq!(l.max_single_tab_width, 20);
    }
}
