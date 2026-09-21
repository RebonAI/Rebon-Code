//! Server list panel — filter / sort / scroll reducer.
//!
//! Tracks:
//! * A search query (from a child search box)
//! * A sort key (`name` / `status` / `scope`)
//! * A scroll offset + selected index
//!
//! Modeled as:
//! * [`ServerListItem`] — flattened display row
//! * [`ServerSortKey`] — sort discriminator
//! * [`ListPanelState`] — reducer state
//! * [`ListPanelEvent`] — reducer event
//! * [`filter_servers`] — pure predicate
//! * [`sort_servers`] — pure comparator
//!
//! The filter predicate is a case-insensitive substring match on
//! `name + scope_label + transport_label` (any of the three match → include).

use crate::runtime::config::{ConfigScope, TransportKind};
use crate::runtime::status::McpServerStatus;

/// A flattened server list row. Pre-built by the consumer from its
/// `ScopedMcpServerConfig` and connection map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerListItem {
    pub name: String,
    pub status: McpServerStatus,
    pub scope: ConfigScope,
    pub transport: TransportKind,
    /// The tool count for the sort-by-tools path. `0` if unknown /
    /// not yet loaded.
    pub tool_count: usize,
    /// Optional error message for sorting stability.
    pub error: Option<String>,
}

/// The sort key. Pinned in the order the sort cycle shifts through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerSortKey {
    /// Alphabetical by name.
    Name,
    /// By status: Connected → Pending → NeedsAuth → Failed → Disabled.
    Status,
    /// By scope: Local → User → Project → Dynamic → Enterprise → ClaudeAi → Managed.
    Scope,
    /// By tool count descending.
    ToolCount,
}

impl ServerSortKey {
    /// The display label. Pinned to the sort cycle below.
    pub fn label(&self) -> &'static str {
        match self {
            ServerSortKey::Name => "name",
            ServerSortKey::Status => "status",
            ServerSortKey::Scope => "scope",
            ServerSortKey::ToolCount => "tool count",
        }
    }

    /// Cycle to the next key. Pinned order: Name → Status → Scope →
    /// ToolCount → Name.
    pub fn next(&self) -> ServerSortKey {
        match self {
            ServerSortKey::Name => ServerSortKey::Status,
            ServerSortKey::Status => ServerSortKey::Scope,
            ServerSortKey::Scope => ServerSortKey::ToolCount,
            ServerSortKey::ToolCount => ServerSortKey::Name,
        }
    }

    pub const ALL: [ServerSortKey; 4] = [
        ServerSortKey::Name,
        ServerSortKey::Status,
        ServerSortKey::Scope,
        ServerSortKey::ToolCount,
    ];
}

/// Filter a server list by a case-insensitive substring match.
///
/// Matches against the concatenation of
/// `name + ' ' + scope_label + ' ' + transport_label` so the user
/// can type `"user http"` to find a user-scoped http server. Empty
/// query returns the full list.
pub fn filter_servers<'a>(items: &'a [ServerListItem], query: &str) -> Vec<&'a ServerListItem> {
    if query.is_empty() {
        return items.iter().collect();
    }
    let needle = query.to_lowercase();
    items
        .iter()
        .filter(|item| {
            let haystack = format!(
                "{} {} {}",
                item.name.to_lowercase(),
                item.scope.as_str(),
                item.transport.as_str()
            );
            haystack.contains(&needle)
        })
        .collect()
}

/// Sort a server list by the given key. Returns a new `Vec` —
/// callers that need in-place sort can build one externally.
///
/// The sort is stable (uses `Vec::sort_by`) so ties preserve input
/// order.
pub fn sort_servers(items: &[ServerListItem], key: ServerSortKey) -> Vec<ServerListItem> {
    let mut out: Vec<ServerListItem> = items.to_vec();
    match key {
        ServerSortKey::Name => {
            out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }
        ServerSortKey::Status => {
            out.sort_by(|a, b| status_rank(a.status).cmp(&status_rank(b.status)))
        }
        ServerSortKey::Scope => out.sort_by(|a, b| scope_rank(a.scope).cmp(&scope_rank(b.scope))),
        ServerSortKey::ToolCount => out.sort_by(|a, b| b.tool_count.cmp(&a.tool_count)),
    }
    out
}

/// Rank table for status sort: Connected first, then Pending,
/// NeedsAuth, Failed, Disabled last.
fn status_rank(s: McpServerStatus) -> u8 {
    match s {
        McpServerStatus::Connected => 0,
        McpServerStatus::Pending => 1,
        McpServerStatus::NeedsAuth => 2,
        McpServerStatus::Failed => 3,
        McpServerStatus::Disabled => 4,
    }
}

/// Rank table for scope sort, pinned to `ConfigScope`'s declaration order.
fn scope_rank(s: ConfigScope) -> u8 {
    match s {
        ConfigScope::Local => 0,
        ConfigScope::User => 1,
        ConfigScope::Project => 2,
        ConfigScope::Dynamic => 3,
        ConfigScope::Enterprise => 4,
        ConfigScope::ClaudeAi => 5,
        ConfigScope::Managed => 6,
    }
}

/// The list panel state: query, sort key, selection, and scroll offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPanelState {
    pub query: String,
    pub sort_key: ServerSortKey,
    pub selected_index: usize,
    pub scroll_offset: usize,
}

impl Default for ListPanelState {
    fn default() -> Self {
        Self {
            query: String::new(),
            sort_key: ServerSortKey::Name,
            selected_index: 0,
            scroll_offset: 0,
        }
    }
}

/// Events for the list panel reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListPanelEvent {
    SetQuery(String),
    CycleSortKey,
    SetSortKey(ServerSortKey),
    SelectNext,
    SelectPrevious,
    SelectIndex(usize),
    /// Apply the filter/sort to the given filtered list length so the
    /// selection can clamp. Must be dispatched after any event that
    /// changes the visible row count.
    ResyncWithListLen(usize),
}

impl ListPanelState {
    /// Apply an event and return the next state.
    pub fn apply(mut self, event: ListPanelEvent, visible_len: usize) -> Self {
        match event {
            ListPanelEvent::SetQuery(q) => {
                self.query = q;
                // Reset selection to 0 so a fresh search doesn't
                // leave the cursor on an out-of-range index.
                self.selected_index = 0;
                self.scroll_offset = 0;
            }
            ListPanelEvent::CycleSortKey => {
                self.sort_key = self.sort_key.next();
            }
            ListPanelEvent::SetSortKey(k) => {
                self.sort_key = k;
            }
            ListPanelEvent::SelectNext => {
                if visible_len > 0 && self.selected_index + 1 < visible_len {
                    self.selected_index += 1;
                }
            }
            ListPanelEvent::SelectPrevious => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
            }
            ListPanelEvent::SelectIndex(i) => {
                self.selected_index = i.min(visible_len.saturating_sub(1));
            }
            ListPanelEvent::ResyncWithListLen(n) => {
                if n == 0 {
                    self.selected_index = 0;
                    self.scroll_offset = 0;
                } else if self.selected_index >= n {
                    self.selected_index = n - 1;
                }
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_item(name: &str, status: McpServerStatus, scope: ConfigScope) -> ServerListItem {
        ServerListItem {
            name: name.to_string(),
            status,
            scope,
            transport: TransportKind::Stdio,
            tool_count: 0,
            error: None,
        }
    }

    // --- filter_servers ---

    #[test]
    fn filter_empty_query_returns_all() {
        let items = vec![
            mk_item("linear", McpServerStatus::Connected, ConfigScope::User),
            mk_item("github", McpServerStatus::Failed, ConfigScope::Local),
        ];
        assert_eq!(filter_servers(&items, "").len(), 2);
    }

    #[test]
    fn filter_by_name_substring() {
        let items = vec![
            mk_item("linear", McpServerStatus::Connected, ConfigScope::User),
            mk_item("github", McpServerStatus::Failed, ConfigScope::Local),
        ];
        let filtered = filter_servers(&items, "lin");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "linear");
    }

    #[test]
    fn filter_is_case_insensitive_on_name() {
        let items = vec![mk_item(
            "Linear",
            McpServerStatus::Connected,
            ConfigScope::User,
        )];
        let filtered = filter_servers(&items, "LIN");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_by_scope_label() {
        let items = vec![
            mk_item("linear", McpServerStatus::Connected, ConfigScope::User),
            mk_item("github", McpServerStatus::Failed, ConfigScope::Local),
        ];
        let filtered = filter_servers(&items, "user");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "linear");
    }

    #[test]
    fn filter_by_transport_label() {
        let mut items = vec![
            mk_item("linear", McpServerStatus::Connected, ConfigScope::User),
            mk_item("github", McpServerStatus::Failed, ConfigScope::Local),
        ];
        items[1].transport = TransportKind::Http;
        let filtered = filter_servers(&items, "http");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "github");
    }

    #[test]
    fn filter_combined_name_and_scope() {
        let items = vec![
            mk_item("linear", McpServerStatus::Connected, ConfigScope::User),
            mk_item("linear2", McpServerStatus::Connected, ConfigScope::Local),
        ];
        let filtered = filter_servers(&items, "linear user");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "linear");
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let items = vec![mk_item(
            "linear",
            McpServerStatus::Connected,
            ConfigScope::User,
        )];
        assert!(filter_servers(&items, "nope").is_empty());
    }

    // --- sort_servers ---

    #[test]
    fn sort_by_name_alphabetical() {
        let items = vec![
            mk_item("zulu", McpServerStatus::Connected, ConfigScope::User),
            mk_item("alpha", McpServerStatus::Connected, ConfigScope::User),
            mk_item("mike", McpServerStatus::Connected, ConfigScope::User),
        ];
        let sorted = sort_servers(&items, ServerSortKey::Name);
        assert_eq!(sorted[0].name, "alpha");
        assert_eq!(sorted[1].name, "mike");
        assert_eq!(sorted[2].name, "zulu");
    }

    #[test]
    fn sort_by_name_case_insensitive() {
        let items = vec![
            mk_item("Zulu", McpServerStatus::Connected, ConfigScope::User),
            mk_item("alpha", McpServerStatus::Connected, ConfigScope::User),
        ];
        let sorted = sort_servers(&items, ServerSortKey::Name);
        assert_eq!(sorted[0].name, "alpha");
        assert_eq!(sorted[1].name, "Zulu");
    }

    #[test]
    fn sort_by_status_connected_first() {
        let items = vec![
            mk_item("a", McpServerStatus::Disabled, ConfigScope::User),
            mk_item("b", McpServerStatus::Failed, ConfigScope::User),
            mk_item("c", McpServerStatus::Connected, ConfigScope::User),
            mk_item("d", McpServerStatus::Pending, ConfigScope::User),
            mk_item("e", McpServerStatus::NeedsAuth, ConfigScope::User),
        ];
        let sorted = sort_servers(&items, ServerSortKey::Status);
        assert_eq!(sorted[0].status, McpServerStatus::Connected);
        assert_eq!(sorted[1].status, McpServerStatus::Pending);
        assert_eq!(sorted[2].status, McpServerStatus::NeedsAuth);
        assert_eq!(sorted[3].status, McpServerStatus::Failed);
        assert_eq!(sorted[4].status, McpServerStatus::Disabled);
    }

    #[test]
    fn sort_by_scope_orders_local_project_managed() {
        let items = vec![
            mk_item("x", McpServerStatus::Connected, ConfigScope::Managed),
            mk_item("y", McpServerStatus::Connected, ConfigScope::Local),
            mk_item("z", McpServerStatus::Connected, ConfigScope::Project),
        ];
        let sorted = sort_servers(&items, ServerSortKey::Scope);
        assert_eq!(sorted[0].scope, ConfigScope::Local);
        assert_eq!(sorted[1].scope, ConfigScope::Project);
        assert_eq!(sorted[2].scope, ConfigScope::Managed);
    }

    #[test]
    fn sort_by_tool_count_descending() {
        let mut items = vec![
            mk_item("a", McpServerStatus::Connected, ConfigScope::User),
            mk_item("b", McpServerStatus::Connected, ConfigScope::User),
            mk_item("c", McpServerStatus::Connected, ConfigScope::User),
        ];
        items[0].tool_count = 5;
        items[1].tool_count = 20;
        items[2].tool_count = 1;
        let sorted = sort_servers(&items, ServerSortKey::ToolCount);
        assert_eq!(sorted[0].name, "b");
        assert_eq!(sorted[1].name, "a");
        assert_eq!(sorted[2].name, "c");
    }

    #[test]
    fn sort_is_stable_on_ties() {
        let items = vec![
            mk_item("a", McpServerStatus::Connected, ConfigScope::User),
            mk_item("b", McpServerStatus::Connected, ConfigScope::User),
            mk_item("c", McpServerStatus::Connected, ConfigScope::User),
        ];
        let sorted = sort_servers(&items, ServerSortKey::Status);
        // All same status → preserve input order.
        assert_eq!(sorted[0].name, "a");
        assert_eq!(sorted[1].name, "b");
        assert_eq!(sorted[2].name, "c");
    }

    #[test]
    fn sort_empty_list_stays_empty() {
        let empty: Vec<ServerListItem> = vec![];
        assert!(sort_servers(&empty, ServerSortKey::Name).is_empty());
    }

    // --- ServerSortKey cycle ---

    #[test]
    fn sort_key_cycle_order() {
        assert_eq!(ServerSortKey::Name.next(), ServerSortKey::Status);
        assert_eq!(ServerSortKey::Status.next(), ServerSortKey::Scope);
        assert_eq!(ServerSortKey::Scope.next(), ServerSortKey::ToolCount);
        assert_eq!(ServerSortKey::ToolCount.next(), ServerSortKey::Name);
    }

    #[test]
    fn sort_key_all_has_four() {
        assert_eq!(ServerSortKey::ALL.len(), 4);
    }

    #[test]
    fn sort_key_labels() {
        assert_eq!(ServerSortKey::Name.label(), "name");
        assert_eq!(ServerSortKey::Status.label(), "status");
        assert_eq!(ServerSortKey::Scope.label(), "scope");
        assert_eq!(ServerSortKey::ToolCount.label(), "tool count");
    }

    // --- ListPanelState reducer ---

    #[test]
    fn state_default() {
        let s = ListPanelState::default();
        assert_eq!(s.query, "");
        assert_eq!(s.sort_key, ServerSortKey::Name);
        assert_eq!(s.selected_index, 0);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn state_set_query_resets_selection() {
        let s = ListPanelState {
            selected_index: 5,
            scroll_offset: 3,
            ..Default::default()
        };
        let next = s.apply(ListPanelEvent::SetQuery("foo".into()), 10);
        assert_eq!(next.query, "foo");
        assert_eq!(next.selected_index, 0);
        assert_eq!(next.scroll_offset, 0);
    }

    #[test]
    fn state_cycle_sort_key() {
        let s = ListPanelState::default();
        let s = s.apply(ListPanelEvent::CycleSortKey, 0);
        assert_eq!(s.sort_key, ServerSortKey::Status);
        let s = s.apply(ListPanelEvent::CycleSortKey, 0);
        assert_eq!(s.sort_key, ServerSortKey::Scope);
    }

    #[test]
    fn state_select_next_clamps_to_visible_len() {
        let s = ListPanelState {
            selected_index: 2,
            ..Default::default()
        };
        let next = s.clone().apply(ListPanelEvent::SelectNext, 5);
        assert_eq!(next.selected_index, 3);
        // At the last row — no movement.
        let last = ListPanelState {
            selected_index: 4,
            ..Default::default()
        };
        let after_last = last.apply(ListPanelEvent::SelectNext, 5);
        assert_eq!(after_last.selected_index, 4);
    }

    #[test]
    fn state_select_previous_clamps_to_zero() {
        let s = ListPanelState {
            selected_index: 0,
            ..Default::default()
        };
        let next = s.apply(ListPanelEvent::SelectPrevious, 5);
        assert_eq!(next.selected_index, 0);
    }

    #[test]
    fn state_select_next_on_empty_list_is_noop() {
        let s = ListPanelState::default();
        let next = s.apply(ListPanelEvent::SelectNext, 0);
        assert_eq!(next.selected_index, 0);
    }

    #[test]
    fn state_select_index_clamps_to_len() {
        let s = ListPanelState::default();
        let next = s.apply(ListPanelEvent::SelectIndex(100), 5);
        assert_eq!(next.selected_index, 4);
    }

    #[test]
    fn state_resync_shrinks_selection() {
        let s = ListPanelState {
            selected_index: 10,
            ..Default::default()
        };
        let next = s.apply(ListPanelEvent::ResyncWithListLen(3), 3);
        assert_eq!(next.selected_index, 2);
    }

    #[test]
    fn state_resync_to_empty_resets_all() {
        let s = ListPanelState {
            selected_index: 5,
            scroll_offset: 3,
            ..Default::default()
        };
        let next = s.apply(ListPanelEvent::ResyncWithListLen(0), 0);
        assert_eq!(next.selected_index, 0);
        assert_eq!(next.scroll_offset, 0);
    }

    #[test]
    fn state_resync_preserves_when_in_range() {
        let s = ListPanelState {
            selected_index: 2,
            ..Default::default()
        };
        let next = s.apply(ListPanelEvent::ResyncWithListLen(5), 5);
        assert_eq!(next.selected_index, 2);
    }

    #[test]
    fn state_set_sort_key_direct() {
        let s = ListPanelState::default();
        let next = s.apply(ListPanelEvent::SetSortKey(ServerSortKey::ToolCount), 10);
        assert_eq!(next.sort_key, ServerSortKey::ToolCount);
    }
}
