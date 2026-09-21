//! Teammate-tree projection for spinner rows.
//!
//! The tree is built vertically and contains:
//!
//! 1. The leader row (`team-lead`) with optional verb / idle text /
//! token count / select-hint / view-hint.
//! 2. One row per running teammate (laid out by [`crate::teammate_line`]).
//! 3. Optionally a `[hide]` footer row when selection mode is on.
//!
//! Selection model: `-1` selects the leader, the teammate count selects
//! the hide row, anything else selects a teammate.

/// Tree-character pairs for the leader row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeChars {
    /// 2-char tree opener.
    pub opener: &'static str,
    /// True when the leader is highlighted (foregrounded or selected).
    pub leader_highlighted: bool,
    /// True when the leader is selected via index `-1`.
    pub leader_selected: bool,
    /// True when the leader is foregrounded (no teammate is being
    /// viewed).
    pub leader_foregrounded: bool,
}

/// The leader row's decoration.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaderRow {
    /// Tree opener (`╒═` when highlighted, `┌─` otherwise).
    pub opener: &'static str,
    /// True when the leader row is highlighted (foregrounded or
    /// selected).
    pub highlighted: bool,
    /// True when the leader is selected via index `-1`.
    pub selected: bool,
    /// True when the leader is foregrounded.
    pub foregrounded: bool,
    /// True when the verb (`": <verb>…"`) should be rendered.
    pub show_verb: bool,
    /// True when the idle text (`": <text>"`) should be rendered.
    /// Mutually exclusive with `show_verb`.
    pub show_idle_text: bool,
    /// True when the leader's token count (`" · N tokens"`) should be
    /// rendered.
    pub show_token_count: bool,
    /// True when the `· shift + ↑/↓ to select` hint should be rendered.
    pub show_select_hint: bool,
    /// True when the `· enter to view` hint should be rendered.
    pub show_view_hint: bool,
}

/// The complete teammate-tree layout for one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct TeammateTreeLayout {
    /// True when the tree should not render at all (no running
    /// teammates).
    pub hidden: bool,
    /// The leader row decoration.
    pub leader: LeaderRow,
    /// True when the trailing `[hide]` footer should render.
    pub show_hide_row: bool,
    /// True when the hide row is selected.
    pub hide_selected: bool,
}

/// Inputs to [`teammate_tree_layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeammateTreeInputs {
    /// How many teammates are running.
    pub teammate_count: usize,
    /// The selected index (`-1` for leader, `len` for hide,
    /// `0..len` for teammates, `None` for "no selection").
    pub selected_index: Option<i64>,
    /// True while the tree is in selection mode.
    pub is_in_selection_mode: bool,
    /// True when no teammate is foregrounded, which foregrounds the
    /// leader instead.
    pub leader_is_foregrounded: bool,
    /// leader verb is present.
    pub has_leader_verb: bool,
    /// leader idle text is present.
    pub has_leader_idle_text: bool,
    /// leader token count is present and greater than zero.
    pub leader_token_count_positive: bool,
}

/// Compute the tree layout for one frame.
pub fn teammate_tree_layout(input: TeammateTreeInputs) -> TeammateTreeLayout {
    if input.teammate_count == 0 {
        return TeammateTreeLayout {
            hidden: true,
            leader: LeaderRow {
                opener: "┌─",
                highlighted: false,
                selected: false,
                foregrounded: false,
                show_verb: false,
                show_idle_text: false,
                show_token_count: false,
                show_select_hint: false,
                show_view_hint: false,
            },
            show_hide_row: false,
            hide_selected: false,
        };
    }

    let leader_selected = input.is_in_selection_mode && input.selected_index == Some(-1);
    let leader_highlighted = input.leader_is_foregrounded || leader_selected;
    let opener = if leader_highlighted {
        "╒═"
    } else {
        "┌─"
    };

    // verb-vs-idle: only render verb when not foregrounded AND have a
    // verb. Render idle text only when not foregrounded AND no verb
    // AND have idle text.
    let show_verb = !input.leader_is_foregrounded && input.has_leader_verb;
    let show_idle_text =
        !input.leader_is_foregrounded && !input.has_leader_verb && input.has_leader_idle_text;
    let show_token_count = input.leader_token_count_positive;
    let show_select_hint = leader_highlighted;
    let show_view_hint = leader_selected && !input.leader_is_foregrounded;

    let hide_selected =
        input.is_in_selection_mode && input.selected_index == Some(input.teammate_count as i64);
    let show_hide_row = input.is_in_selection_mode;

    TeammateTreeLayout {
        hidden: false,
        leader: LeaderRow {
            opener,
            highlighted: leader_highlighted,
            selected: leader_selected,
            foregrounded: input.leader_is_foregrounded,
            show_verb,
            show_idle_text,
            show_token_count,
            show_select_hint,
            show_view_hint,
        },
        show_hide_row,
        hide_selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> TeammateTreeInputs {
        TeammateTreeInputs {
            teammate_count: 2,
            selected_index: None,
            is_in_selection_mode: false,
            leader_is_foregrounded: true,
            has_leader_verb: false,
            has_leader_idle_text: false,
            leader_token_count_positive: false,
        }
    }

    #[test]
    fn empty_teammates_returns_hidden() {
        let mut i = base();
        i.teammate_count = 0;
        let r = teammate_tree_layout(i);
        assert!(r.hidden);
    }

    #[test]
    fn leader_foregrounded_no_selection_highlighted() {
        let i = base();
        let r = teammate_tree_layout(i);
        assert!(!r.hidden);
        assert!(r.leader.highlighted);
        assert!(r.leader.foregrounded);
        assert_eq!(r.leader.opener, "╒═");
    }

    #[test]
    fn leader_not_foregrounded_no_selection_dim() {
        let mut i = base();
        i.leader_is_foregrounded = false;
        let r = teammate_tree_layout(i);
        assert!(!r.leader.highlighted);
        assert_eq!(r.leader.opener, "┌─");
    }

    #[test]
    fn leader_selected_via_minus_one() {
        let mut i = base();
        i.is_in_selection_mode = true;
        i.selected_index = Some(-1);
        i.leader_is_foregrounded = false;
        let r = teammate_tree_layout(i);
        assert!(r.leader.selected);
        assert!(r.leader.highlighted);
        assert_eq!(r.leader.opener, "╒═");
    }

    #[test]
    fn show_verb_when_not_foregrounded_with_verb() {
        let mut i = base();
        i.leader_is_foregrounded = false;
        i.has_leader_verb = true;
        let r = teammate_tree_layout(i);
        assert!(r.leader.show_verb);
        assert!(!r.leader.show_idle_text);
    }

    #[test]
    fn show_idle_text_when_no_verb() {
        let mut i = base();
        i.leader_is_foregrounded = false;
        i.has_leader_verb = false;
        i.has_leader_idle_text = true;
        let r = teammate_tree_layout(i);
        assert!(r.leader.show_idle_text);
        assert!(!r.leader.show_verb);
    }

    #[test]
    fn no_verb_or_idle_when_foregrounded() {
        let mut i = base();
        i.has_leader_verb = true;
        i.has_leader_idle_text = true;
        let r = teammate_tree_layout(i);
        assert!(!r.leader.show_verb);
        assert!(!r.leader.show_idle_text);
    }

    #[test]
    fn token_count_visible_when_positive() {
        let mut i = base();
        i.leader_token_count_positive = true;
        let r = teammate_tree_layout(i);
        assert!(r.leader.show_token_count);
    }

    #[test]
    fn token_count_hidden_when_zero() {
        let r = teammate_tree_layout(base());
        assert!(!r.leader.show_token_count);
    }

    #[test]
    fn select_hint_only_when_highlighted() {
        // Foregrounded → highlighted → hint shown.
        let r = teammate_tree_layout(base());
        assert!(r.leader.show_select_hint);
    }

    #[test]
    fn select_hint_hidden_when_not_highlighted() {
        let mut i = base();
        i.leader_is_foregrounded = false;
        let r = teammate_tree_layout(i);
        assert!(!r.leader.show_select_hint);
    }

    #[test]
    fn view_hint_only_when_selected_not_foregrounded() {
        let mut i = base();
        i.is_in_selection_mode = true;
        i.selected_index = Some(-1);
        i.leader_is_foregrounded = false;
        let r = teammate_tree_layout(i);
        assert!(r.leader.show_view_hint);
    }

    #[test]
    fn view_hint_hidden_when_foregrounded() {
        let mut i = base();
        i.is_in_selection_mode = true;
        i.selected_index = Some(-1);
        // Stays foregrounded.
        let r = teammate_tree_layout(i);
        assert!(!r.leader.show_view_hint);
    }

    #[test]
    fn hide_row_present_in_selection_mode() {
        let mut i = base();
        i.is_in_selection_mode = true;
        let r = teammate_tree_layout(i);
        assert!(r.show_hide_row);
    }

    #[test]
    fn hide_row_selected_when_index_at_count() {
        let mut i = base();
        i.is_in_selection_mode = true;
        i.selected_index = Some(2); // teammate_count == 2
        let r = teammate_tree_layout(i);
        assert!(r.show_hide_row);
        assert!(r.hide_selected);
    }

    #[test]
    fn hide_row_not_selected_when_index_at_teammate() {
        let mut i = base();
        i.is_in_selection_mode = true;
        i.selected_index = Some(0);
        let r = teammate_tree_layout(i);
        assert!(r.show_hide_row);
        assert!(!r.hide_selected);
    }

    #[test]
    fn hide_row_absent_outside_selection_mode() {
        let r = teammate_tree_layout(base());
        assert!(!r.show_hide_row);
    }
}
