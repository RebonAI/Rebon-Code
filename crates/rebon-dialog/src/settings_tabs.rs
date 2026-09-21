//! Settings tab dispatcher.
//!
//! ## Pinned rules
//!
//! 1. **Tab order is `Status, Config, Usage`; `Gates` is defined but
//!    never registered in this build.** Pinned by [`BUILT_IN_TABS`].
//! 2. **`initial_header_focused` is true unless the default tab is `Config`
//!    or `Gates`.** Config has interactive content, so the header starts
//!    unfocused there. Pinned by [`initial_header_focused`].
//! 3. **Esc ownership is per tab**, carried as `tabs_hidden`,
//!    `config_owns_esc` and `gates_owns_esc` on [`TabState`], so the
//!    consumer can derive its activation predicate from one value instead of
//!    re-deriving it per tab.

/// Which settings tab a pane is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabId {
    /// Read-only facts about the session and the build.
    Status,
    /// The editable option rows.
    Config,
    /// Cost and rate-limit reporting.
    Usage,
    /// The Gates tab, which this build does not register.
    Gates,
}

impl TabId {
    /// The tab's name as it is written down and compared.
    pub fn as_wire(&self) -> &'static str {
        match self {
            TabId::Status => "Status",
            TabId::Config => "Config",
            TabId::Usage => "Usage",
            TabId::Gates => "Gates",
        }
    }
}

/// The default three-tab list. Pinned by tests.
pub const BUILT_IN_TABS: [TabId; 3] = [TabId::Status, TabId::Config, TabId::Usage];

/// Whether the tab header starts focused: true for every default tab
/// except [`TabId::Config`] and [`TabId::Gates`].
pub fn initial_header_focused(default_tab: TabId) -> bool {
    !matches!(default_tab, TabId::Config | TabId::Gates)
}

/// Settings-pane state. The per-tab Esc-cession flags are modeled as a
/// single struct so the consumer's reducer can swap state without
/// re-deriving the activation predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabState {
    /// The tab on screen.
    pub selected_tab: TabId,
    /// Whether the tab strip itself is hidden.
    pub tabs_hidden: bool,
    /// Whether the Config tab has taken Esc for its own editing.
    pub config_owns_esc: bool,
    /// Whether the Gates tab has taken Esc for its own editing.
    pub gates_owns_esc: bool,
}

impl TabState {
    /// Open on `default_tab`, with the tab strip shown and Esc unclaimed.
    pub fn new(default_tab: TabId) -> Self {
        Self {
            selected_tab: default_tab,
            tabs_hidden: false,
            config_owns_esc: false,
            gates_owns_esc: false,
        }
    }
}

/// Events the dispatcher can apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabEvent {
    /// User picked a tab from the header.
    SelectTab(TabId),
    /// Submenu opened or closed in a child tab; sets
    /// `TabState::tabs_hidden`.
    SetTabsHidden(bool),
    /// Config child tab toggled its own Esc handler; sets
    /// `TabState::config_owns_esc`.
    SetConfigOwnsEsc(bool),
    /// Gates child tab toggled its own Esc handler; sets
    /// `TabState::gates_owns_esc`.
    SetGatesOwnsEsc(bool),
}

/// Apply a tab event to the state. Returns the new state.
pub fn apply_tab_event(state: &TabState, event: TabEvent) -> TabState {
    let mut next = state.clone();
    match event {
        TabEvent::SelectTab(t) => next.selected_tab = t,
        TabEvent::SetTabsHidden(h) => next.tabs_hidden = h,
        TabEvent::SetConfigOwnsEsc(b) => next.config_owns_esc = b,
        TabEvent::SetGatesOwnsEsc(b) => next.gates_owns_esc = b,
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_tabs_constant_pinned() {
        assert_eq!(BUILT_IN_TABS, [TabId::Status, TabId::Config, TabId::Usage]);
    }

    #[test]
    fn tab_id_wire_strings() {
        assert_eq!(TabId::Status.as_wire(), "Status");
        assert_eq!(TabId::Config.as_wire(), "Config");
        assert_eq!(TabId::Usage.as_wire(), "Usage");
        assert_eq!(TabId::Gates.as_wire(), "Gates");
    }

    // ---- initial_header_focused ----

    #[test]
    fn initial_header_focused_status() {
        assert!(initial_header_focused(TabId::Status));
    }

    #[test]
    fn initial_header_focused_usage() {
        assert!(initial_header_focused(TabId::Usage));
    }

    #[test]
    fn initial_header_focused_config_unfocused() {
        assert!(!initial_header_focused(TabId::Config));
    }

    #[test]
    fn initial_header_focused_gates_unfocused() {
        assert!(!initial_header_focused(TabId::Gates));
    }

    // ---- TabState reducer ----

    #[test]
    fn tab_state_default_starts_visible() {
        let s = TabState::new(TabId::Status);
        assert_eq!(s.selected_tab, TabId::Status);
        assert!(!s.tabs_hidden);
        assert!(!s.config_owns_esc);
        assert!(!s.gates_owns_esc);
    }

    #[test]
    fn apply_select_tab() {
        let s = TabState::new(TabId::Status);
        let next = apply_tab_event(&s, TabEvent::SelectTab(TabId::Config));
        assert_eq!(next.selected_tab, TabId::Config);
        assert!(!next.tabs_hidden);
    }

    #[test]
    fn apply_set_tabs_hidden() {
        let s = TabState::new(TabId::Status);
        let next = apply_tab_event(&s, TabEvent::SetTabsHidden(true));
        assert!(next.tabs_hidden);
    }

    #[test]
    fn apply_config_owns_esc() {
        let s = TabState::new(TabId::Config);
        let next = apply_tab_event(&s, TabEvent::SetConfigOwnsEsc(true));
        assert!(next.config_owns_esc);
        assert!(!next.gates_owns_esc);
    }

    #[test]
    fn apply_gates_owns_esc() {
        let s = TabState::new(TabId::Gates);
        let next = apply_tab_event(&s, TabEvent::SetGatesOwnsEsc(true));
        assert!(next.gates_owns_esc);
    }
}
