//! The agent-group tools (RFC-0009), offered to whichever agent started this
//! server.
//!
//! The tools themselves are `rebon-group`'s; this is where they meet the
//! MCP connection. The one thing this side adds is who is calling: the
//! session this server was started for, found once from the environment its
//! agent gave it, or — when that says nothing — from what the agent tells
//! `group_join`, kept for the rest of the connection.

use std::path::Path;
use std::sync::Mutex;

use rebon_group::tools::{self, ToolContext};
use rebon_group::{Caller, GroupStore};
use serde_json::{json, Value};

pub(crate) struct GroupDesk {
    store: GroupStore,
    /// The directory the server was started in, as text: groups are made
    /// here and joined from here.
    root: String,
    caller: Mutex<Option<Caller>>,
}

impl GroupDesk {
    pub(crate) fn new(store: GroupStore, root: &Path, caller: Option<Caller>) -> Self {
        Self {
            store,
            root: root.to_string_lossy().into_owned(),
            caller: Mutex::new(caller),
        }
    }

    pub(crate) fn offers(name: &str) -> bool {
        tools::specs().iter().any(|spec| spec.name == name)
    }

    /// Whether this server offers the group tools at all. Not to a Rebon
    /// session: its `groups` plugin already gave it the same tools, run as
    /// itself, and a second copy under the MCP prefix would only be a way to
    /// get them wrong. A Rebon session with that plugin off is out of groups.
    ///
    /// Nor to an agent program switched out of groups (`agents.json`), or to
    /// anyone while that file cannot be read. A caller not yet known gets the
    /// tools, and `group_join` refuses it if its program is switched out.
    pub(crate) fn offered(&self) -> bool {
        let caller = self
            .caller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(caller) = caller.as_ref() else {
            return true;
        };
        caller.agent != rebon_group::identity::AgentKind::REBON
            && self
                .store
                .agent_policy()
                .is_ok_and(|policy| policy.allows(&caller.agent))
    }

    /// The group tools as `tools/list` entries.
    pub(crate) fn list() -> Vec<Value> {
        tools::specs()
            .into_iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "inputSchema": spec.input_schema,
                    "annotations": {
                        "readOnlyHint": spec.read_only,
                        "openWorldHint": false,
                    },
                })
            })
            .collect()
    }

    /// Runs one group tool. Blocking: it takes the group's file lock.
    ///
    /// `meta` is the call's `_meta`: an agent that keeps its session out of
    /// its servers' environment may name it there (Codex does), and then
    /// that is who is calling.
    pub(crate) fn call(
        &self,
        name: &str,
        arguments: Value,
        meta: Option<&Value>,
    ) -> anyhow::Result<Value> {
        if !self.offered() {
            anyhow::bail!(
                "this session is not offered the group tools: a Rebon session has its own                  (the groups plugin), and an agent program switched out of groups                  (Rebon desktop: Settings > Groups) has none"
            );
        }
        let mut caller = self
            .caller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if caller.is_none() {
            *caller = meta.and_then(rebon_group::identity::from_call_meta);
        }
        tools::call(
            &mut ToolContext {
                store: &self.store,
                caller: &mut caller,
                root: &self.root,
            },
            name,
            arguments,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_group::identity::AgentKind;

    fn desk(dir: &Path, agent: Option<&str>) -> GroupDesk {
        GroupDesk::new(
            GroupStore::new(dir.join("groups")),
            dir,
            agent.map(|agent| Caller {
                agent: agent.to_string(),
                session_id: "s1".to_string(),
            }),
        )
    }

    #[test]
    fn the_tools_follow_each_agents_switch() {
        let dir = tempfile::tempdir().unwrap();
        assert!(desk(dir.path(), Some(AgentKind::CODEX)).offered());
        assert!(desk(dir.path(), Some(AgentKind::CLAUDE_CODE)).offered());

        GroupStore::new(dir.path().join("groups"))
            .set_agent_allowed(AgentKind::CODEX, false)
            .unwrap();
        let codex = desk(dir.path(), Some(AgentKind::CODEX));
        assert!(!codex.offered());
        let refused = codex
            .call(rebon_group::tools::GROUP_INFO, json!({}), None)
            .unwrap_err();
        assert!(refused.to_string().contains("switched out"), "{refused:#}");
        assert!(desk(dir.path(), Some(AgentKind::CLAUDE_CODE)).offered());
    }

    #[test]
    fn a_rebon_session_never_gets_the_mcp_copy() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!desk(dir.path(), Some(AgentKind::REBON)).offered());
    }

    #[test]
    fn an_unknown_caller_is_offered_and_its_join_is_gated() {
        let dir = tempfile::tempdir().unwrap();
        GroupStore::new(dir.path().join("groups"))
            .set_agent_allowed(AgentKind::CODEX, false)
            .unwrap();
        let unknown = desk(dir.path(), None);
        assert!(unknown.offered());
        let refused = unknown
            .call(
                rebon_group::tools::GROUP_JOIN,
                json!({ "group": "g", "agent": AgentKind::CODEX, "session_id": "x1" }),
                None,
            )
            .unwrap_err();
        assert!(refused.to_string().contains("switched out"), "{refused:#}");
    }

    #[test]
    fn a_damaged_switch_file_offers_nobody() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("groups")).unwrap();
        std::fs::write(dir.path().join("groups/agents.json"), "nope").unwrap();
        assert!(!desk(dir.path(), Some(AgentKind::CLAUDE_CODE)).offered());
    }
}
