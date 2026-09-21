/// Header focus a fresh tabs surface starts with.
pub const DEFAULT_INITIAL_HEADER_FOCUSED: bool = true;
/// Whether the content region may drive tab navigation by default.
pub const DEFAULT_NAV_FROM_CONTENT: bool = false;

/// Action ID of the next-tab keybinding.
pub const ACTION_TABS_NEXT: &str = "tabs:next";
/// Action ID of the previous-tab keybinding.
pub const ACTION_TABS_PREVIOUS: &str = "tabs:previous";
/// Keybinding context tabs register their keys under: `"Tabs"`.
pub const KEYBINDING_CONTEXT: &str = "Tabs";

/// Reducer holding the selected tab together with the focus and
/// enablement flags that gate navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabsState {
    /// Index of the selected tab.
    pub selected_index: usize,
    /// True when the header row has focus rather than the content region.
    pub header_focused: bool,
    /// True when a parent layout has hidden the tabs.
    pub hidden: bool,
    /// True when keyboard navigation is off, e.g. because a child is bound
    /// to the same keys.
    pub disable_navigation: bool,
}

impl TabsState {
    /// Build a fresh state, resolving `default_tab` against `tab_ids`.
    /// A `None` or unknown id falls back to index 0.
    pub fn new(tab_ids: &[&str], default_tab: Option<&str>, initial_header_focused: bool) -> Self {
        let selected_index = match default_tab {
            Some(name) => tab_ids.iter().position(|t| *t == name).unwrap_or(0),
            None => 0,
        };
        Self {
            selected_index,
            header_focused: initial_header_focused,
            hidden: false,
            disable_navigation: false,
        }
    }

    /// Apply an event against a tab list of `tab_count` entries. Returns
    /// true when anything changed.
    pub fn step(&mut self, event: TabsEvent, tab_count: usize) -> bool {
        let old = self.clone();
        match event {
            TabsEvent::Next => {
                self.cycle(1, tab_count);
            }
            TabsEvent::Previous => {
                self.cycle(-1, tab_count);
            }
            TabsEvent::FocusHeader => {
                self.header_focused = true;
            }
            TabsEvent::BlurHeader => {
                self.header_focused = false;
            }
            TabsEvent::SelectByIndex(idx) => {
                if idx < tab_count {
                    self.selected_index = idx;
                    self.header_focused = true;
                }
            }
        }
        *self != old
    }

    /// Move the selection by `offset`, wrapping around. Selecting always
    /// focuses the header.
    fn cycle(&mut self, offset: i32, tab_count: usize) {
        if tab_count == 0 {
            return;
        }
        // (selected_index + tab_count + offset) % tab_count
        // Adding `tab_count + offset` handles negative offsets
        // correctly, since Rust's `%` on signed ints can return a
        // negative remainder otherwise.
        let n = tab_count as i64;
        let cur = self.selected_index as i64;
        let new_idx = ((cur + n + offset as i64) % n) as usize;
        self.selected_index = new_idx;
        self.header_focused = true;
    }

    /// True when keybinding navigation should fire on the header: the tabs
    /// are visible, navigation is enabled and the header holds focus.
    pub fn header_navigation_active(&self) -> bool {
        !self.hidden && !self.disable_navigation && self.header_focused
    }

    /// True when keybinding navigation should fire from the content region.
    ///
    /// `nav_from_content` and `opted_in` are the caller's runtime values;
    /// the remaining conditions come from this state's own flags.
    pub fn content_navigation_active(&self, nav_from_content: bool, opted_in: bool) -> bool {
        nav_from_content
            && !self.header_focused
            && opted_in
            && !self.hidden
            && !self.disable_navigation
    }
}

/// Navigation and focus events the tabs reducer accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabsEvent {
    /// `tabs:next` — cycle forward, wrapping.
    Next,
    /// `tabs:previous` — cycle backward, wrapping.
    Previous,
    /// Give the header row focus.
    FocusHeader,
    /// Move focus off the header, i.e. into the content.
    BlurHeader,
    /// Jump straight to a tab index. An out-of-range index is ignored.
    SelectByIndex(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<&'static str> {
        vec!["a", "b", "c", "d"]
    }

    #[test]
    fn defaults_pinned() {
        assert_eq!(DEFAULT_INITIAL_HEADER_FOCUSED, true);
        assert_eq!(DEFAULT_NAV_FROM_CONTENT, false);
        assert_eq!(ACTION_TABS_NEXT, "tabs:next");
        assert_eq!(ACTION_TABS_PREVIOUS, "tabs:previous");
        assert_eq!(KEYBINDING_CONTEXT, "Tabs");
    }

    // ────────────────────────────────────────────────────────────────
    // new() / default tab lookup
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn new_defaults_to_index_zero_when_no_default_tab() {
        let s = TabsState::new(&ids(), None, true);
        assert_eq!(s.selected_index, 0);
        assert!(s.header_focused);
    }

    #[test]
    fn new_resolves_default_tab_by_name() {
        let s = TabsState::new(&ids(), Some("c"), true);
        assert_eq!(s.selected_index, 2);
    }

    #[test]
    fn new_falls_back_to_zero_on_unknown_default_tab() {
        // `default_tab_index !== -1 ? default_tab_index : 0`
        let s = TabsState::new(&ids(), Some("not-a-tab"), true);
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn new_initial_header_focused_propagates() {
        let s = TabsState::new(&ids(), None, false);
        assert!(!s.header_focused);
    }

    // ────────────────────────────────────────────────────────────────
    // Navigation cycling
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn next_increments_index() {
        let mut s = TabsState::new(&ids(), None, true);
        s.step(TabsEvent::Next, 4);
        assert_eq!(s.selected_index, 1);
    }

    #[test]
    fn next_wraps_at_end() {
        let mut s = TabsState::new(&ids(), Some("d"), true);
        s.step(TabsEvent::Next, 4);
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn previous_decrements_index() {
        let mut s = TabsState::new(&ids(), Some("c"), true);
        s.step(TabsEvent::Previous, 4);
        assert_eq!(s.selected_index, 1);
    }

    #[test]
    fn previous_wraps_at_start() {
        let mut s = TabsState::new(&ids(), None, true);
        s.step(TabsEvent::Previous, 4);
        assert_eq!(s.selected_index, 3);
    }

    #[test]
    fn next_then_previous_returns_to_origin() {
        let mut s = TabsState::new(&ids(), Some("b"), true);
        s.step(TabsEvent::Next, 4);
        assert_eq!(s.selected_index, 2);
        s.step(TabsEvent::Previous, 4);
        assert_eq!(s.selected_index, 1);
    }

    #[test]
    fn navigation_focuses_header() {
        let mut s = TabsState::new(&ids(), None, false);
        assert!(!s.header_focused);
        s.step(TabsEvent::Next, 4);
        assert!(s.header_focused);
    }

    #[test]
    fn navigation_with_zero_tabs_is_no_op() {
        let mut s = TabsState::new(&[], None, true);
        s.step(TabsEvent::Next, 0);
        assert_eq!(s.selected_index, 0);
    }

    #[test]
    fn navigation_with_one_tab_stays_at_zero() {
        let mut s = TabsState::new(&["only"], None, true);
        s.step(TabsEvent::Next, 1);
        assert_eq!(s.selected_index, 0);
        s.step(TabsEvent::Previous, 1);
        assert_eq!(s.selected_index, 0);
    }

    // ────────────────────────────────────────────────────────────────
    // Header focus events
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn focus_header_event() {
        let mut s = TabsState::new(&ids(), None, false);
        s.step(TabsEvent::FocusHeader, 4);
        assert!(s.header_focused);
    }

    #[test]
    fn blur_header_event() {
        let mut s = TabsState::new(&ids(), None, true);
        s.step(TabsEvent::BlurHeader, 4);
        assert!(!s.header_focused);
    }

    // ────────────────────────────────────────────────────────────────
    // Direct selection
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn select_by_index_in_range() {
        let mut s = TabsState::new(&ids(), None, false);
        s.step(TabsEvent::SelectByIndex(2), 4);
        assert_eq!(s.selected_index, 2);
        assert!(s.header_focused);
    }

    #[test]
    fn select_by_out_of_range_index_is_no_op() {
        let mut s = TabsState::new(&ids(), None, true);
        s.step(TabsEvent::SelectByIndex(10), 4);
        assert_eq!(s.selected_index, 0);
    }

    // ────────────────────────────────────────────────────────────────
    // Header navigation active
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn header_navigation_active_when_focused_visible_enabled() {
        let s = TabsState::new(&ids(), None, true);
        assert!(s.header_navigation_active());
    }

    #[test]
    fn header_navigation_inactive_when_hidden() {
        let mut s = TabsState::new(&ids(), None, true);
        s.hidden = true;
        assert!(!s.header_navigation_active());
    }

    #[test]
    fn header_navigation_inactive_when_disabled() {
        let mut s = TabsState::new(&ids(), None, true);
        s.disable_navigation = true;
        assert!(!s.header_navigation_active());
    }

    #[test]
    fn header_navigation_inactive_when_blurred() {
        let s = TabsState::new(&ids(), None, false);
        assert!(!s.header_navigation_active());
    }

    // ────────────────────────────────────────────────────────────────
    // Content navigation active
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn content_navigation_requires_all_conditions() {
        let s = TabsState::new(&ids(), None, false); // header NOT focused
                                                     // nav_from_content=true, opted_in=true, not hidden, not disabled
        assert!(s.content_navigation_active(true, true));
    }

    #[test]
    fn content_navigation_inactive_when_header_focused() {
        let s = TabsState::new(&ids(), None, true);
        assert!(!s.content_navigation_active(true, true));
    }

    #[test]
    fn content_navigation_inactive_when_not_opted_in() {
        let s = TabsState::new(&ids(), None, false);
        assert!(!s.content_navigation_active(true, false));
    }

    #[test]
    fn content_navigation_inactive_when_nav_from_content_false() {
        let s = TabsState::new(&ids(), None, false);
        assert!(!s.content_navigation_active(false, true));
    }

    #[test]
    fn step_returns_change_flag() {
        let mut s = TabsState::new(&ids(), None, true);
        let changed = s.step(TabsEvent::Next, 4);
        assert!(changed);
        // Re-running BlurHeader from blurred state should report no change.
        s.header_focused = false;
        let changed2 = s.step(TabsEvent::BlurHeader, 4);
        assert!(!changed2);
    }

    // ────────────────────────────────────────────────────────────────
    // table for cycling
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn tab_cycle_wraps_table() {
        // Match the expression:
        // (selected_tab_index + tabs.length + offset) % tabs.length
        let n = 5;
        let cases: &[(usize, i32, usize)] = &[
            (0, 1, 1),
            (4, 1, 0),
            (0, -1, 4),
            (3, -1, 2),
            (2, 1, 3),
            (2, -1, 1),
        ];
        for (start, offset, expected) in cases {
            let mut s = TabsState {
                selected_index: *start,
                header_focused: true,
                hidden: false,
                disable_navigation: false,
            };
            let evt = if *offset == 1 {
                TabsEvent::Next
            } else {
                TabsEvent::Previous
            };
            s.step(evt, n);
            assert_eq!(s.selected_index, *expected, "start={start} offset={offset}");
        }
    }
}
