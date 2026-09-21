//! Per-teammate row layout for teammate spinner rows.
//!
//! The layout logic is the progressive-width gating block.
//! Inputs are the per-teammate state, terminal width, selection /
//! foreground flags, and the resolved activity text. Outputs are:
//!
//! * which tree character to use (`├─` / `└─` / `╞═` / `╘═`)
//! * whether the agent name is shown
//! * which extras (stats, select-hint, view-hint) are shown
//! * the maximum width the activity text can take
//!
//! Resolving the activity text itself — the recent activities, then a
//! summary of them, then the last activity, then a random verb — is
//! **caller-resolved**, because it needs the activity store and width
//! truncation, both of which the caller already has.

use crate::TEAMMATE_SELECT_HINT;

/// The fixed-width prefix the layout reserves before the agent name.
/// `left padding(3) + pointer(1) + space(1) + tree char(2) + space(1) = 8`.
pub const BASE_PREFIX_WIDTH: usize = 8;

/// The minimum number of cells the activity text must have for the
/// row to be considered "wide enough".
pub const MIN_ACTIVITY_WIDTH: usize = 25;

/// The minimum terminal width before the agent name is shown.
pub const MIN_FULL_NAME_COLUMNS: usize = 60;

/// The per-teammate state the layout reads.
#[derive(Debug, Clone, PartialEq)]
pub struct TeammateInfo {
    /// The agent's name, used for the `@name:` prefix.
    pub agent_name: String,
    /// Whether the teammate is idle — selects the idle / past-tense
    /// status branch.
    pub is_idle: bool,
    /// Whether shutdown was requested — adds the `[stopping]` status.
    pub shutdown_requested: bool,
    /// Whether plan approval is pending — adds the `[awaiting approval]`
    /// status.
    pub awaiting_plan_approval: bool,
    /// Completed tool-use count. Used by the stats string.
    pub tool_use_count: u64,
    /// Token count. Used by the stats string.
    pub token_count: u64,
    /// Pre-resolved activity text. It is built from the recent
    /// activities, then a summary of them, then the last activity, then
    /// a random verb. The caller does that lookup before invoking the
    /// layout.
    pub activity_text: String,
}

/// Same shape as [`TeammateInfo`] but as a typed enum so the renderer
/// doesn't need to re-derive the priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateStatusKind {
    /// `[stopping]` — shutdown requested.
    Stopping,
    /// `[awaiting approval]` — plan approval pending.
    AwaitingApproval,
    /// `Idle for X` (when not all idle) or `<past-tense verb> for X` (when
    /// all idle).
    Idle,
    /// Active — render the activity text. Suppressed when highlighted.
    Active,
}

/// The progress counters of a teammate, exposed for tests / docs.
#[derive(Debug, Clone, PartialEq)]
pub struct TeammateProgress {
    /// Completed tool-use count.
    pub tool_use_count: u64,
    /// Token count.
    pub token_count: u64,
}

/// The full layout result for one teammate row.
#[derive(Debug, Clone, PartialEq)]
pub struct TeammateLineLayout {
    /// The 2-cell tree character: `├─`, `└─`, `╞═`, `╘═`.
    pub tree_char: &'static str,
    /// True when the agent name should be rendered before the
    /// activity text.
    pub show_name: bool,
    /// True when the stats (`· N tool uses · N tokens`) string is
    /// rendered.
    pub show_stats: bool,
    /// True when the `· shift + ↑/↓ to select` hint is rendered.
    pub show_select_hint: bool,
    /// True when the `· enter to view` hint is rendered.
    pub show_view_hint: bool,
    /// Maximum visual width the activity text may take.
    pub activity_max_width: usize,
    /// Which status branch to render.
    pub status_kind: TeammateStatusKind,
}

/// Inputs to [`teammate_line_layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeammateLineInputs {
    /// Terminal width in columns.
    pub columns: usize,
    /// True when this is the last row in the tree (chooses `└─`/`╘═`).
    pub is_last: bool,
    /// True when this row is selected (highlighted by user navigation).
    pub is_selected: bool,
    /// True when this row is the foregrounded teammate (currently
    /// being viewed).
    pub is_foregrounded: bool,
    /// True when the row is shutting down.
    pub shutdown_requested: bool,
    /// True when the teammate is awaiting plan approval.
    pub awaiting_plan_approval: bool,
    /// True when the teammate is idle.
    pub is_idle: bool,
    /// Visual width of the agent name including the `@` prefix.
    pub full_name_width: usize,
    /// Visual width of the formatted stats string, i.e. of
    /// ` · N tool uses · N tokens` as rendered for this teammate. The
    /// caller computes it once per render: the default 0-tool / 0-token form
    /// is 25 cells, but the real width grows with the digit counts of both
    /// fields, so the gating threshold cannot be a constant if it is to track
    /// the per-render width exactly.
    pub stats_width: usize,
}

/// Compute the layout for one teammate row.
pub fn teammate_line_layout(input: TeammateLineInputs) -> TeammateLineLayout {
    let is_highlighted = input.is_selected || input.is_foregrounded;
    let tree_char = match (is_highlighted, input.is_last) {
        (true, true) => "╘═",
        (true, false) => "╞═",
        (false, true) => "└─",
        (false, false) => "├─",
    };

    let select_hint_width = TEAMMATE_SELECT_HINT.chars().count() + 3; // ` · ` prefix
    let view_hint_width = " · enter to view".chars().count();
    let stats_width = input.stats_width;

    let space_with_full_name = input
        .columns
        .saturating_sub(BASE_PREFIX_WIDTH)
        .saturating_sub(input.full_name_width)
        .saturating_sub(2);
    let show_name =
        input.columns >= MIN_FULL_NAME_COLUMNS && space_with_full_name >= MIN_ACTIVITY_WIDTH;
    let name_width = if show_name {
        input.full_name_width + 2
    } else {
        0
    };

    let available_for_activity = input
        .columns
        .saturating_sub(BASE_PREFIX_WIDTH)
        .saturating_sub(name_width);

    // Progressive hiding: view hint → select hint → stats.
    let show_view_hint = input.is_selected
        && !input.is_foregrounded
        && available_for_activity > view_hint_width + stats_width + MIN_ACTIVITY_WIDTH + 5;
    let show_select_hint = is_highlighted
        && available_for_activity
            > select_hint_width
                + (if show_view_hint { view_hint_width } else { 0 })
                + stats_width
                + MIN_ACTIVITY_WIDTH
                + 5;
    let show_stats = available_for_activity > stats_width + MIN_ACTIVITY_WIDTH + 5;

    let extras_cost = (if show_stats { stats_width } else { 0 })
        + (if show_select_hint {
            select_hint_width
        } else {
            0
        })
        + (if show_view_hint { view_hint_width } else { 0 });
    let activity_max_width = MIN_ACTIVITY_WIDTH.max(
        available_for_activity
            .saturating_sub(extras_cost)
            .saturating_sub(1),
    );

    let status_kind = if input.shutdown_requested {
        TeammateStatusKind::Stopping
    } else if input.awaiting_plan_approval {
        TeammateStatusKind::AwaitingApproval
    } else if input.is_idle {
        TeammateStatusKind::Idle
    } else {
        TeammateStatusKind::Active
    };

    TeammateLineLayout {
        tree_char,
        show_name,
        show_stats,
        show_select_hint,
        show_view_hint,
        activity_max_width,
        status_kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> TeammateLineInputs {
        TeammateLineInputs {
            columns: 100,
            is_last: false,
            is_selected: false,
            is_foregrounded: false,
            shutdown_requested: false,
            awaiting_plan_approval: false,
            is_idle: false,
            full_name_width: 10,
            // Default to the previously-hardcoded approximation so the
            // existing test gating thresholds remain meaningful.
            stats_width: 30,
        }
    }

    #[test]
    fn tree_char_default_branch() {
        let r = teammate_line_layout(base());
        assert_eq!(r.tree_char, "├─");
    }

    #[test]
    fn tree_char_last_unselected() {
        let mut i = base();
        i.is_last = true;
        let r = teammate_line_layout(i);
        assert_eq!(r.tree_char, "└─");
    }

    #[test]
    fn tree_char_selected_not_last() {
        let mut i = base();
        i.is_selected = true;
        let r = teammate_line_layout(i);
        assert_eq!(r.tree_char, "╞═");
    }

    #[test]
    fn tree_char_selected_last() {
        let mut i = base();
        i.is_selected = true;
        i.is_last = true;
        let r = teammate_line_layout(i);
        assert_eq!(r.tree_char, "╘═");
    }

    #[test]
    fn tree_char_foregrounded_treated_as_highlighted() {
        let mut i = base();
        i.is_foregrounded = true;
        let r = teammate_line_layout(i);
        assert_eq!(r.tree_char, "╞═");
    }

    #[test]
    fn show_name_on_wide_terminal() {
        let i = base();
        let r = teammate_line_layout(i);
        assert!(r.show_name);
    }

    #[test]
    fn hide_name_on_narrow_terminal() {
        let mut i = base();
        i.columns = 50;
        let r = teammate_line_layout(i);
        assert!(!r.show_name);
    }

    #[test]
    fn hide_name_when_long_name_does_not_fit() {
        let mut i = base();
        i.columns = 60;
        i.full_name_width = 30;
        // base 8 + name 30 + 2 = 40; 60 - 40 = 20 < 25 → hide name.
        let r = teammate_line_layout(i);
        assert!(!r.show_name);
    }

    #[test]
    fn stats_shown_on_wide_terminal() {
        let r = teammate_line_layout(base());
        assert!(r.show_stats);
    }

    #[test]
    fn stats_hidden_on_narrow_terminal() {
        let mut i = base();
        i.columns = 50;
        let r = teammate_line_layout(i);
        // After hiding the name, the activity has more room, but
        // 50 - 8 = 42 - 30 - 25 - 5 = -26 → not enough.
        assert!(!r.show_stats);
    }

    #[test]
    fn select_hint_only_when_highlighted() {
        let mut i = base();
        // Need a wide enough terminal to fit name + activity + stats
        // + view-hint + select-hint.
        i.columns = 200;
        i.is_selected = true;
        let r = teammate_line_layout(i);
        assert!(r.show_select_hint);
    }

    #[test]
    fn select_hint_hidden_when_not_highlighted() {
        let r = teammate_line_layout(base());
        assert!(!r.show_select_hint);
    }

    #[test]
    fn view_hint_only_when_selected_not_foregrounded() {
        let mut i = base();
        i.is_selected = true;
        let r = teammate_line_layout(i);
        assert!(r.show_view_hint);
    }

    #[test]
    fn view_hint_hidden_when_foregrounded() {
        let mut i = base();
        i.is_selected = true;
        i.is_foregrounded = true;
        let r = teammate_line_layout(i);
        assert!(!r.show_view_hint);
    }

    #[test]
    fn status_kind_priority_stopping() {
        let mut i = base();
        i.shutdown_requested = true;
        i.awaiting_plan_approval = true;
        i.is_idle = true;
        let r = teammate_line_layout(i);
        assert_eq!(r.status_kind, TeammateStatusKind::Stopping);
    }

    #[test]
    fn status_kind_priority_awaiting_approval() {
        let mut i = base();
        i.awaiting_plan_approval = true;
        i.is_idle = true;
        let r = teammate_line_layout(i);
        assert_eq!(r.status_kind, TeammateStatusKind::AwaitingApproval);
    }

    #[test]
    fn status_kind_idle() {
        let mut i = base();
        i.is_idle = true;
        let r = teammate_line_layout(i);
        assert_eq!(r.status_kind, TeammateStatusKind::Idle);
    }

    #[test]
    fn status_kind_active_default() {
        let r = teammate_line_layout(base());
        assert_eq!(r.status_kind, TeammateStatusKind::Active);
    }

    #[test]
    fn activity_max_width_at_least_minimum() {
        let mut i = base();
        i.columns = 30;
        let r = teammate_line_layout(i);
        assert!(r.activity_max_width >= MIN_ACTIVITY_WIDTH);
    }

    #[test]
    fn activity_max_width_grows_with_terminal() {
        let mut narrow = base();
        narrow.columns = 80;
        let mut wide = base();
        wide.columns = 200;
        let n = teammate_line_layout(narrow);
        let w = teammate_line_layout(wide);
        assert!(w.activity_max_width >= n.activity_max_width);
    }

    #[test]
    fn stats_width_input_drives_gating_threshold() {
        // The minimal stats string ` · 0 tool uses · 0 tokens` is 25
        // cells; the maximal `· N tool uses · N.Mk tokens` for a
        // many-digit teammate is closer to 33. Pick a column count
        // where the small stats width fits but the large one does
        // not, and verify the input actually drives the decision.
        //
        // available_for_activity = columns - 8 (no name shown).
        // cols=50 -> available=42; need stats_width + 25 + 5 + 1 < 42,
        // so stats_width <= 10. 10 fits, 12 does not.
        let mut small = base();
        small.columns = 50;
        small.full_name_width = 30; // forces show_name=false
        small.stats_width = 10;
        let r_small = teammate_line_layout(small);
        assert!(r_small.show_stats, "stats_width=10 must fit at cols=50");

        let mut large = base();
        large.columns = 50;
        large.full_name_width = 30;
        large.stats_width = 12;
        let r_large = teammate_line_layout(large);
        assert!(
            !r_large.show_stats,
            "stats_width=12 must NOT fit at cols=50"
        );
    }

    #[test]
    fn stats_width_input_affects_extras_cost() {
        // When stats are shown, the gating math charges `stats_width`
        // to `extras_cost`, which directly subtracts from
        // activity_max_width. Larger stats_width means a smaller
        // activity budget; pin that relationship.
        let mut small = base();
        small.columns = 200;
        small.stats_width = 20;
        let mut large = base();
        large.columns = 200;
        large.stats_width = 50;
        let r_small = teammate_line_layout(small);
        let r_large = teammate_line_layout(large);
        assert!(r_small.show_stats && r_large.show_stats);
        assert!(
            r_small.activity_max_width > r_large.activity_max_width,
            "smaller stats_width should leave more activity room \
             ({} vs {})",
            r_small.activity_max_width,
            r_large.activity_max_width,
        );
    }

    #[test]
    fn stats_width_zero_does_not_panic() {
        let mut i = base();
        i.stats_width = 0;
        // With zero stats width, the show_stats threshold is just
        // MIN_ACTIVITY_WIDTH + 5 = 30 cells of activity room.
        let r = teammate_line_layout(i);
        // 100 cols, no name (assume show_name based on default), then
        // make sure we don't panic and that gating still produces a
        // sensible answer.
        let _ = r.show_stats;
    }
}
