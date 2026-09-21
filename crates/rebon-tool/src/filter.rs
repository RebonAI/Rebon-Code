//! `ToolFilter` — composable tool-visibility filter.
//!
//! Tool filtering is needed in several places, each of which would
//! otherwise carry its own ad-hoc logic:
//!
//! 1. **Coordinator mode** — the main session is restricted to the
//!    tools a coordinator may call, with internal worker tools held back.
//! 2. **Sub-agent spawning**
//!    passes an `allowed_tools` list so the sub-agent sees only a
//!    subset.
//! 3. **Per-session configuration** — some workflows want to
//!    disable specific tools (e.g. `Bash`) without removing them
//!    from the engine registry.
//! 4. **Policy-driven filtering** — settings files may declare
//!    `allowed_tools` / `denied_tools` arrays.
//!
//! This module provides one shared type, [`ToolFilter`], that
//! every call site can use. The filter is:
//!
//! - **Composable** — `allow_only(...)` + `with_deny(...)` combine
//!   naturally, and two filters can be [`ToolFilter::intersect`]ed
//!   to produce the most-restrictive union.
//! - **Alias-aware** — tool lists often mix canonical and legacy
//!   names (`Read` vs `FileReadTool`). The filter resolves both
//!   through [`crate::tool_matches_name`]-style logic.
//! - **Case-insensitive** — configured tool lists are inconsistent about
//!   casing; the filter normalises.
//!
//! Filtering is **non-destructive**: applying a filter to a tool
//! list returns a new list without mutating the source.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use rebon_tools_core::tool_matches_name;

/// Tool visibility rule.
///
/// Applied to any list of tool names or tool snapshots to produce
/// a filtered list. The rules are:
///
/// - If `allow` is `None`, every tool is visible by default.
/// - If `allow` is `Some(list)`, only tools whose name matches an
///   entry in the list are visible.
/// - `deny` always wins: tools in the deny set are hidden even if
///   `allow` would include them.
/// - Name matching uses [`matches_tool_name`] which accepts both
///   canonical and legacy tool identifiers (`Read`/`FileReadTool`,
///   `Bash`/`BashTool`, etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolFilter {
    allow: Option<BTreeSet<String>>,
    deny: BTreeSet<String>,
}

impl ToolFilter {
    /// A filter that allows every tool.
    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// A filter that denies every tool (useful for sub-agents that
    /// must run model-only with no tool-use).
    pub fn deny_all() -> Self {
        Self {
            allow: Some(BTreeSet::new()),
            deny: BTreeSet::new(),
        }
    }

    /// A filter that only allows the listed tool names.
    ///
    /// Duplicate or alias entries are coalesced. An empty iterator
    /// is equivalent to [`Self::deny_all`].
    pub fn allow_only<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: BTreeSet<String> = tools.into_iter().map(Into::into).collect();
        Self {
            allow: Some(set),
            deny: BTreeSet::new(),
        }
    }

    /// Add tools to the deny list (builder-style).
    pub fn with_deny<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny.extend(tools.into_iter().map(Into::into));
        self
    }

    /// Append tools to the allow list. Creates the allow list if
    /// it was `None`.
    pub fn with_allow<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set = self.allow.get_or_insert_with(BTreeSet::new);
        set.extend(tools.into_iter().map(Into::into));
        self
    }

    /// Build from a serializable request-bound spec.
    pub fn from_spec(spec: rebon_types::ToolFilterSpec) -> Self {
        Self {
            allow: spec.allow,
            deny: spec.deny,
        }
    }

    /// Convert to a dependency-light serializable spec.
    pub fn to_spec(&self) -> rebon_types::ToolFilterSpec {
        rebon_types::ToolFilterSpec {
            allow: self.allow.clone(),
            deny: self.deny.clone(),
        }
    }

    /// Produce the intersection of `self` and `other` — the most
    /// restrictive combination. Used when a parent filter (e.g.
    /// coordinator mode) needs to be combined with a sub-agent's
    /// own `allowed_tools`.
    pub fn intersect(&self, other: &ToolFilter) -> Self {
        let allow = match (&self.allow, &other.allow) {
            (None, None) => None,
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (Some(a), Some(b)) => Some(a.intersection(b).cloned().collect()),
        };
        let deny: BTreeSet<String> = self.deny.union(&other.deny).cloned().collect();
        Self { allow, deny }
    }

    /// Whether the filter has any restriction at all.
    pub fn is_unrestricted(&self) -> bool {
        self.allow.is_none() && self.deny.is_empty()
    }

    /// Return a human-readable description of the allowed tools.
    ///
    /// Used by `format_agent_line` to describe what tools an agent has access
    /// to, and by the profile prompts to describe the surface a user is about
    /// to approve.
    ///
    /// Denied entries are subtracted from the allow list rather than listed
    /// alongside it. `deny` always wins in [`Self::allows`], so naming a tool
    /// here that the filter rejects would describe a surface that does not
    /// exist — which matters most where the two lists meet, as they do when a
    /// profile's allow list is intersected with a session that already denies
    /// something.
    pub fn describe_allowed(&self) -> String {
        match &self.allow {
            Some(list) if !list.is_empty() => {
                let names: Vec<&str> = list
                    .iter()
                    .filter(|entry| !self.deny.contains(*entry))
                    .map(|s| s.as_str())
                    .collect();
                if names.is_empty() {
                    return "None".to_string();
                }
                names.join(", ")
            }
            _ if !self.deny.is_empty() => {
                let denied: Vec<&str> = self.deny.iter().map(|s| s.as_str()).collect();
                format!("All tools except {}", denied.join(", "))
            }
            _ => "*".to_string(),
        }
    }

    /// Test whether a tool name (and its aliases) passes the
    /// filter.
    ///
    /// The `aliases` list is the same one callers get from
    /// [`crate::Tool::aliases`] — the filter accepts the tool when
    /// any of those names matches an allow entry and none matches
    /// a deny entry.
    pub fn allows(&self, tool_name: &str, aliases: &[&'static str]) -> bool {
        if self.deny_hits(tool_name, aliases) {
            return false;
        }
        match &self.allow {
            None => true,
            Some(list) => list
                .iter()
                .any(|entry| tool_matches_name(tool_name, aliases, entry)),
        }
    }

    fn deny_hits(&self, tool_name: &str, aliases: &[&'static str]) -> bool {
        self.deny
            .iter()
            .any(|entry| tool_matches_name(tool_name, aliases, entry))
    }

    /// Snapshot the current allow list (or `None` if unrestricted).
    /// Primarily for diagnostics and tests.
    pub fn allow_list(&self) -> Option<Vec<String>> {
        self.allow.as_ref().map(|set| set.iter().cloned().collect())
    }

    /// Snapshot the current deny list.
    pub fn deny_list(&self) -> Vec<String> {
        self.deny.iter().cloned().collect()
    }
}

/// Runtime-swappable [`ToolFilter`] handle.
///
/// Wraps an `Arc<RwLock<ToolFilter>>` so multiple clones can share
/// a single mutable filter cell. Used by places (e.g. the TUI
/// `/ceo` toggle) that need to refresh the filter on an already-
/// running executor or sub-agent spawner without rebuilding them.
///
/// Cloning produces another handle that points at the same cell,
/// so a [`set`](Self::set) call through one clone is visible to
/// every other clone.
#[derive(Clone, Debug, Default)]
pub struct SharedToolFilter {
    inner: Arc<RwLock<ToolFilter>>,
}

impl SharedToolFilter {
    /// Wrap an initial filter in a new shared cell.
    pub fn new(filter: ToolFilter) -> Self {
        Self {
            inner: Arc::new(RwLock::new(filter)),
        }
    }

    /// Clone the current filter out of the cell. Done on every tool-
    /// dispatch path so the latest filter is always seen.
    pub fn current(&self) -> ToolFilter {
        self.inner
            .read()
            .expect("shared tool filter lock poisoned")
            .clone()
    }

    /// Overwrite the shared filter. Any clone of this handle will
    /// observe the new value on its next [`current`](Self::current)
    /// call.
    pub fn set(&self, filter: ToolFilter) {
        *self
            .inner
            .write()
            .expect("shared tool filter lock poisoned") = filter;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_filter_allows_any_tool() {
        let f = ToolFilter::unrestricted();
        assert!(f.allows("Bash", &["BashTool"]));
        assert!(f.allows("Anything", &[]));
        assert!(f.is_unrestricted());
    }

    #[test]
    fn deny_all_rejects_every_tool() {
        let f = ToolFilter::deny_all();
        assert!(!f.allows("Bash", &["BashTool"]));
        assert!(!f.allows("Read", &["FileReadTool"]));
        assert!(!f.is_unrestricted());
    }

    #[test]
    fn allow_only_restricts_to_listed_tools() {
        let f = ToolFilter::allow_only(["Bash", "Read"]);
        assert!(f.allows("Bash", &["BashTool"]));
        assert!(f.allows("Read", &["FileReadTool"]));
        assert!(!f.allows("Write", &["FileWriteTool"]));
    }

    #[test]
    fn allow_matches_via_aliases() {
        let f = ToolFilter::allow_only(["FileReadTool"]);
        assert!(f.allows("Read", &["FileReadTool"]));
    }

    #[test]
    fn deny_overrides_allow() {
        let f = ToolFilter::allow_only(["Bash", "Read"]).with_deny(["Bash"]);
        assert!(!f.allows("Bash", &["BashTool"]));
        assert!(f.allows("Read", &["FileReadTool"]));
    }

    #[test]
    fn deny_applies_when_allow_is_unrestricted() {
        let f = ToolFilter::unrestricted().with_deny(["Bash"]);
        assert!(!f.allows("Bash", &["BashTool"]));
        assert!(f.allows("Read", &["FileReadTool"]));
    }

    #[test]
    fn with_allow_merges_into_existing_set() {
        let f = ToolFilter::allow_only(["Read"]).with_allow(["Write"]);
        assert!(f.allows("Read", &["FileReadTool"]));
        assert!(f.allows("Write", &["FileWriteTool"]));
        assert!(!f.allows("Bash", &["BashTool"]));
    }

    #[test]
    fn intersect_narrows_allow_lists() {
        let parent = ToolFilter::allow_only(["Bash", "Read", "Write"]);
        let child = ToolFilter::allow_only(["Read", "Grep"]);
        let combined = parent.intersect(&child);
        assert!(combined.allows("Read", &["FileReadTool"]));
        assert!(!combined.allows("Bash", &["BashTool"]));
        assert!(!combined.allows("Grep", &["GrepTool"]));
    }

    #[test]
    fn intersect_unions_deny_lists() {
        let a = ToolFilter::unrestricted().with_deny(["Bash"]);
        let b = ToolFilter::unrestricted().with_deny(["Write"]);
        let combined = a.intersect(&b);
        assert!(!combined.allows("Bash", &["BashTool"]));
        assert!(!combined.allows("Write", &["FileWriteTool"]));
        assert!(combined.allows("Read", &["FileReadTool"]));
    }

    #[test]
    fn intersect_one_unrestricted_returns_the_other() {
        let restricted = ToolFilter::allow_only(["Read"]);
        let combined = ToolFilter::unrestricted().intersect(&restricted);
        assert!(combined.allows("Read", &["FileReadTool"]));
        assert!(!combined.allows("Write", &["FileWriteTool"]));
    }

    #[test]
    fn allow_list_snapshot_returns_sorted_entries() {
        let f = ToolFilter::allow_only(["Write", "Read", "Bash"]);
        let list = f.allow_list().unwrap();
        assert_eq!(
            list,
            vec!["Bash".to_string(), "Read".to_string(), "Write".to_string()]
        );
    }

    #[test]
    fn deny_list_snapshot_returns_sorted_entries() {
        let f = ToolFilter::unrestricted().with_deny(["Write", "Bash"]);
        let deny = f.deny_list();
        assert_eq!(deny, vec!["Bash".to_string(), "Write".to_string()]);
    }

    #[test]
    fn from_iterator_produces_allow_only_filter() {
        let f = ToolFilter::allow_only(std::iter::empty::<String>());
        assert!(!f.allows("Any", &[]));
    }

    #[test]
    fn shared_filter_reflects_latest_value_after_set() {
        let handle = SharedToolFilter::new(ToolFilter::unrestricted());
        assert!(handle.current().allows("Bash", &["BashTool"]));

        handle.set(ToolFilter::allow_only(["Read"]));
        assert!(handle.current().allows("Read", &["FileReadTool"]));
        assert!(!handle.current().allows("Bash", &["BashTool"]));
    }

    #[test]
    fn shared_filter_clones_share_the_same_cell() {
        let a = SharedToolFilter::new(ToolFilter::unrestricted());
        let b = a.clone();

        a.set(ToolFilter::deny_all());
        assert!(!b.current().allows("Bash", &["BashTool"]));
        assert!(!a.current().allows("Read", &["FileReadTool"]));
    }

    #[test]
    fn shared_filter_default_is_unrestricted() {
        let handle = SharedToolFilter::default();
        assert!(handle.current().is_unrestricted());
    }
}
