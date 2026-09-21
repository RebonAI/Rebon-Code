//! Agents-list reducer ([`AgentsListState`]).
//!
//! Three pure pieces:
//!
//! 1. **Sorting** — [`compare_agents_by_name`] (lexicographic on
//!    `agent_type`).
//! 2. **Filtering** — built-in agents are split into their own
//!    section; the "selectable" list omits them. When the source filter
//!    is `AgentSourceFilter::All`,
//!    the selectable list is grouped by source in the
//!    `AGENT_SOURCE_GROUPS` order; otherwise it's just the non-built-in
//!    agents flat.
//! 3. **Selection navigation** — up/down arrows wrap, with the
//!    pseudo "Create new agent" row (when present) at the top of the
//!    cycle.
//!
//! All three are modeled as pure functions / a small
//! reducer. The display projection (override info, model display,
//! shadowed-by labels) is a separate value-shape the consumer
//! computes upstream.

use crate::surface::types::{AgentSource, AgentSummary, SettingSource};
use crate::surface::utils::AgentSourceFilter;

/// Pinned source-group order for the grouped list view.
pub const AGENT_SOURCE_GROUPS: &[SettingSource] = &[
    SettingSource::ProjectSettings,
    SettingSource::LocalSettings,
    SettingSource::UserSettings,
    SettingSource::PolicySettings,
    SettingSource::FlagSettings,
];

/// Lexicographic comparator over `agent_type`.
pub fn compare_agents_by_name(a: &AgentSummary, b: &AgentSummary) -> std::cmp::Ordering {
    a.agent_type.cmp(&b.agent_type)
}

/// Sort an agent list by `agent_type` (stable, ascending).
pub fn sort_agents(mut agents: Vec<AgentSummary>) -> Vec<AgentSummary> {
    agents.sort_by(compare_agents_by_name);
    agents
}

/// Compute the "selectable in order" list (non-built-in agents,
/// grouped by source when the filter is `AgentSourceFilter::All`).
pub fn selectable_in_order(
    sorted: &[AgentSummary],
    source: &AgentSourceFilter,
) -> Vec<AgentSummary> {
    let non_built_in: Vec<&AgentSummary> = sorted.iter().filter(|a| !a.is_built_in()).collect();

    if matches!(source, AgentSourceFilter::All) {
        let mut out: Vec<AgentSummary> = Vec::with_capacity(non_built_in.len());
        for group_source in AGENT_SOURCE_GROUPS {
            for a in non_built_in.iter() {
                if let AgentSource::Settings(s) = &a.source {
                    if s == group_source {
                        out.push((*a).clone());
                    }
                }
            }
        }
        return out;
    }

    non_built_in.iter().map(|a| (*a).clone()).collect()
}

/// Built-in subset (always at the bottom of the list).
pub fn built_in_agents(sorted: &[AgentSummary]) -> Vec<AgentSummary> {
    sorted.iter().filter(|a| a.is_built_in()).cloned().collect()
}

/// Reducer state — which row is highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsListState {
    /// True iff the pseudo "Create new agent" row is highlighted.
    pub create_new_selected: bool,
    /// Currently-highlighted agent (if any).
    pub selected: Option<AgentSummary>,
    /// Whether the consumer wired a `Create new agent` callback.
    pub has_create_option: bool,
    /// The list of selectable agents in display order.
    pub selectable: Vec<AgentSummary>,
}

impl AgentsListState {
    /// Build a fresh state. The pseudo "Create new agent" row starts
    /// highlighted when present.
    pub fn new(selectable: Vec<AgentSummary>, has_create_option: bool) -> Self {
        AgentsListState {
            create_new_selected: has_create_option,
            selected: if has_create_option {
                None
            } else {
                selectable.first().cloned()
            },
            has_create_option,
            selectable,
        }
    }

    /// Apply an event. Pure reducer.
    pub fn handle_event(self, event: AgentsListEvent) -> ListOutcome {
        match event {
            AgentsListEvent::Up => ListOutcome::Continue(self.move_cursor(-1)),
            AgentsListEvent::Down => ListOutcome::Continue(self.move_cursor(1)),
            AgentsListEvent::Select => {
                if self.create_new_selected && self.has_create_option {
                    return ListOutcome::CreateNew;
                }
                if let Some(a) = &self.selected {
                    return ListOutcome::Selected(a.clone());
                }
                ListOutcome::Continue(self)
            }
        }
    }

    /// Cycle the cursor by `delta` (+1 = down, -1 = up). The cycle
    /// includes the pseudo "Create new" row at index 0 when present.
    fn move_cursor(self, delta: isize) -> Self {
        let total_items = self.selectable.len() + (if self.has_create_option { 1 } else { 0 });
        if total_items == 0 {
            return self;
        }
        let mut current_position: isize = 0;
        if !self.create_new_selected {
            if let Some(sel) = &self.selected {
                if let Some(idx) = self
                    .selectable
                    .iter()
                    .position(|a| a.agent_type == sel.agent_type && a.source == sel.source)
                {
                    current_position = if self.has_create_option {
                        idx as isize + 1
                    } else {
                        idx as isize
                    };
                }
            }
        }
        let total = total_items as isize;
        let new_position = if delta < 0 {
            if current_position == 0 {
                total - 1
            } else {
                current_position - 1
            }
        } else if current_position == total - 1 {
            0
        } else {
            current_position + 1
        };
        let mut state = self;
        if state.has_create_option && new_position == 0 {
            state.create_new_selected = true;
            state.selected = None;
        } else {
            let agent_index = if state.has_create_option {
                new_position - 1
            } else {
                new_position
            };
            if let Some(a) = state.selectable.get(agent_index as usize) {
                state.create_new_selected = false;
                state.selected = Some(a.clone());
            }
        }
        state
    }
}

/// Outcome of an event applied to [`AgentsListState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListOutcome {
    /// State updated; no upstream action.
    Continue(AgentsListState),
    /// User picked the "Create new agent" pseudo-row.
    CreateNew,
    /// User picked a real agent.
    Selected(AgentSummary),
}

/// Events the reducer accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentsListEvent {
    /// Up arrow (wraps to bottom from top).
    Up,
    /// Down arrow (wraps to top from bottom).
    Down,
    /// Enter key — confirm selection.
    Select,
}

/// Computed display title for the list — delegates to
/// [`crate::surface::utils::agent_source_display_name`].
pub fn list_title(source: &AgentSourceFilter) -> &'static str {
    crate::surface::utils::agent_source_display_name(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, source: AgentSource) -> AgentSummary {
        AgentSummary::minimal(name, "use", "system prompt long enough", source)
    }

    #[test]
    fn sort_agents_alphabetic() {
        let agents = vec![
            agent("foo", AgentSource::Settings(SettingSource::UserSettings)),
            agent("bar", AgentSource::Settings(SettingSource::UserSettings)),
            agent("baz", AgentSource::Settings(SettingSource::UserSettings)),
        ];
        let sorted = sort_agents(agents);
        assert_eq!(sorted[0].agent_type, "bar");
        assert_eq!(sorted[1].agent_type, "baz");
        assert_eq!(sorted[2].agent_type, "foo");
    }

    #[test]
    fn selectable_filters_built_in() {
        let agents = vec![
            agent("a", AgentSource::BuiltIn),
            agent("b", AgentSource::Settings(SettingSource::UserSettings)),
        ];
        let s = selectable_in_order(&agents, &AgentSourceFilter::All);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].agent_type, "b");
    }

    #[test]
    fn selectable_groups_by_source_when_all() {
        let agents = vec![
            agent("u", AgentSource::Settings(SettingSource::UserSettings)),
            agent("p", AgentSource::Settings(SettingSource::ProjectSettings)),
            agent("l", AgentSource::Settings(SettingSource::LocalSettings)),
        ];
        let s = selectable_in_order(&agents, &AgentSourceFilter::All);
        // ProjectSettings comes first per AGENT_SOURCE_GROUPS.
        assert_eq!(s[0].agent_type, "p");
        assert_eq!(s[1].agent_type, "l");
        assert_eq!(s[2].agent_type, "u");
    }

    #[test]
    fn selectable_does_not_regroup_when_specific_source() {
        let agents = vec![
            agent("u", AgentSource::Settings(SettingSource::UserSettings)),
            agent("p", AgentSource::Settings(SettingSource::ProjectSettings)),
        ];
        let s = selectable_in_order(
            &agents,
            &AgentSourceFilter::Source(SettingSource::UserSettings),
        );
        // No grouping; original order preserved.
        assert_eq!(s[0].agent_type, "u");
        assert_eq!(s[1].agent_type, "p");
    }

    #[test]
    fn built_in_agents_subset() {
        let agents = vec![
            agent("a", AgentSource::BuiltIn),
            agent("b", AgentSource::Settings(SettingSource::UserSettings)),
            agent("c", AgentSource::BuiltIn),
        ];
        let bi = built_in_agents(&agents);
        assert_eq!(bi.len(), 2);
    }

    // ---- reducer ----

    #[test]
    fn new_state_with_create_option_starts_on_create() {
        let s = AgentsListState::new(vec![], true);
        assert!(s.create_new_selected);
        assert!(s.selected.is_none());
    }

    #[test]
    fn new_state_without_create_option_starts_on_first() {
        let agents = vec![agent(
            "a",
            AgentSource::Settings(SettingSource::UserSettings),
        )];
        let s = AgentsListState::new(agents.clone(), false);
        assert!(!s.create_new_selected);
        assert_eq!(s.selected.unwrap().agent_type, "a");
    }

    #[test]
    fn down_from_create_advances_to_first() {
        let agents = vec![agent(
            "a",
            AgentSource::Settings(SettingSource::UserSettings),
        )];
        let s = AgentsListState::new(agents.clone(), true);
        let s = match s.handle_event(AgentsListEvent::Down) {
            ListOutcome::Continue(s) => s,
            other => panic!("expected Continue, got {other:?}"),
        };
        assert!(!s.create_new_selected);
        assert_eq!(s.selected.unwrap().agent_type, "a");
    }

    #[test]
    fn down_at_end_wraps_to_create() {
        let agents = vec![
            agent("a", AgentSource::Settings(SettingSource::UserSettings)),
            agent("b", AgentSource::Settings(SettingSource::UserSettings)),
        ];
        let s = AgentsListState::new(agents.clone(), true);
        let s = match s.handle_event(AgentsListEvent::Down) {
            ListOutcome::Continue(s) => s,
            _ => panic!(),
        };
        let s = match s.handle_event(AgentsListEvent::Down) {
            ListOutcome::Continue(s) => s,
            _ => panic!(),
        };
        // Now on b. Down again wraps to Create.
        let s = match s.handle_event(AgentsListEvent::Down) {
            ListOutcome::Continue(s) => s,
            _ => panic!(),
        };
        assert!(s.create_new_selected);
    }

    #[test]
    fn up_from_create_wraps_to_last() {
        let agents = vec![
            agent("a", AgentSource::Settings(SettingSource::UserSettings)),
            agent("b", AgentSource::Settings(SettingSource::UserSettings)),
        ];
        let s = AgentsListState::new(agents.clone(), true);
        let s = match s.handle_event(AgentsListEvent::Up) {
            ListOutcome::Continue(s) => s,
            _ => panic!(),
        };
        assert_eq!(s.selected.unwrap().agent_type, "b");
    }

    #[test]
    fn select_create_returns_create_new() {
        let s = AgentsListState::new(vec![], true);
        let outcome = s.handle_event(AgentsListEvent::Select);
        assert_eq!(outcome, ListOutcome::CreateNew);
    }

    #[test]
    fn select_agent_returns_selected() {
        let agents = vec![agent(
            "a",
            AgentSource::Settings(SettingSource::UserSettings),
        )];
        let mut s = AgentsListState::new(agents.clone(), false);
        s.selected = Some(agents[0].clone());
        let outcome = s.handle_event(AgentsListEvent::Select);
        match outcome {
            ListOutcome::Selected(a) => assert_eq!(a.agent_type, "a"),
            _ => panic!(),
        }
    }

    #[test]
    fn empty_list_no_create_select_no_op() {
        let s = AgentsListState::new(vec![], false);
        let outcome = s.handle_event(AgentsListEvent::Select);
        assert!(matches!(outcome, ListOutcome::Continue(_)));
    }

    #[test]
    fn list_title_uses_display_name() {
        assert_eq!(list_title(&AgentSourceFilter::All), "Agents");
        assert_eq!(list_title(&AgentSourceFilter::BuiltIn), "Built-in agents");
    }
}
