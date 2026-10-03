//! Which agent programs may be in groups at all — the desktop app's
//! 设置 > 群组 switches.
//!
//! One file under the groups root, `agents.json`, names the programs
//! switched out: `{"excluded": ["codex"]}`. No file means every program may
//! join. [`GroupStore::join`] is the gate, so every way in — `rebon mcp
//! serve`, the `groups` plugin on a Rebon session, the desktop app's own
//! pickers — is refused alike; `rebon mcp serve` also stops listing the
//! tools to a program switched out, once it can tell who is calling.
//!
//! Switching a program out does not take its sessions out of the groups
//! they are already in. The app does that, after asking whether their
//! sessions go too, because only it can ask.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store::{write_json, GroupStore};

const AGENTS_FILE: &str = "agents.json";

/// `agents.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPolicy {
    /// Agent programs (`AgentKind` names) that may not join a group.
    #[serde(default)]
    pub excluded: BTreeSet<String>,
}

impl AgentPolicy {
    pub fn allows(&self, agent: &str) -> bool {
        !self.excluded.contains(agent)
    }
}

impl GroupStore {
    /// The switches as they stand. A missing file is everyone in; a file
    /// that does not parse is an error, so the gate in `join` fails closed
    /// rather than reading a damaged switch as on.
    pub fn agent_policy(&self) -> Result<AgentPolicy> {
        let path = self.root().join(AGENTS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(AgentPolicy::default())
            }
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Lets `agent` into groups, or keeps it out of new ones.
    pub fn set_agent_allowed(&self, agent: &str, allowed: bool) -> Result<AgentPolicy> {
        let mut policy = self.agent_policy()?;
        if allowed {
            policy.excluded.remove(agent);
        } else {
            policy.excluded.insert(agent.to_string());
        }
        std::fs::create_dir_all(self.root())
            .with_context(|| format!("failed to create {}", self.root().display()))?;
        write_json(&self.root().join(AGENTS_FILE), &policy)?;
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::AgentKind;
    use crate::model::{Delivery, Member};

    fn member(agent: &str, session_id: &str) -> Member {
        Member {
            agent: agent.into(),
            session_id: session_id.into(),
            alias: format!("{agent}-{session_id}"),
            role: None,
            delivery: Delivery::Auto,
            joined_at_ms: 1,
        }
    }

    #[test]
    fn with_no_file_every_agent_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path().join("groups"));
        let policy = store.agent_policy().unwrap();
        assert_eq!(policy, AgentPolicy::default());
        for agent in [
            AgentKind::REBON,
            AgentKind::CLAUDE_CODE,
            AgentKind::CODEX,
            "unknown",
        ] {
            assert!(policy.allows(agent), "{agent}");
        }
    }

    #[test]
    fn switching_out_and_back_in_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        // The root does not exist yet: the first switch creates it.
        let store = GroupStore::new(dir.path().join("groups"));
        let out = store.set_agent_allowed(AgentKind::CODEX, false).unwrap();
        assert!(!out.allows(AgentKind::CODEX));
        assert!(out.allows(AgentKind::CLAUDE_CODE));
        assert_eq!(store.agent_policy().unwrap(), out);
        let text = std::fs::read_to_string(store.root().join(AGENTS_FILE)).unwrap();
        assert!(text.contains("\"excluded\""), "{text}");

        // Twice out is still once in the set.
        store.set_agent_allowed(AgentKind::CODEX, false).unwrap();
        assert_eq!(store.agent_policy().unwrap().excluded.len(), 1);

        let back = store.set_agent_allowed(AgentKind::CODEX, true).unwrap();
        assert!(back.allows(AgentKind::CODEX));
        assert_eq!(store.agent_policy().unwrap(), AgentPolicy::default());
    }

    #[test]
    fn a_switched_out_agent_cannot_join_and_the_others_still_can() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("refactor", "/work/app").unwrap();
        store
            .set_agent_allowed(AgentKind::CLAUDE_CODE, false)
            .unwrap();

        let refused = store
            .join(&group.id, member(AgentKind::CLAUDE_CODE, "c1"))
            .unwrap_err();
        assert!(
            refused.to_string().contains(AgentKind::CLAUDE_CODE),
            "{refused:#}"
        );
        assert!(store.load(&group.id).unwrap().members.is_empty());

        store
            .join(&group.id, member(AgentKind::CODEX, "x1"))
            .unwrap();
        store
            .set_agent_allowed(AgentKind::CLAUDE_CODE, true)
            .unwrap();
        store
            .join(&group.id, member(AgentKind::CLAUDE_CODE, "c1"))
            .unwrap();
        assert_eq!(store.load(&group.id).unwrap().members.len(), 2);
    }

    #[test]
    fn every_known_agent_can_be_admitted_excluded_and_reenabled() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("clients", "/work/app").unwrap();
        for &agent in AgentKind::KNOWN {
            store.set_agent_allowed(agent, false).unwrap();
            assert!(
                store.join(&group.id, member(agent, "s1")).is_err(),
                "{agent}"
            );
            assert!(store.load(&group.id).unwrap().members.is_empty());
            store.set_agent_allowed(agent, true).unwrap();
            let joining = member(agent, "s1");
            store.join(&group.id, joining.clone()).unwrap();
            assert_eq!(store.load(&group.id).unwrap().members[0].agent, agent);
            store.leave(&group.id, &joining.key()).unwrap();
        }
        assert_eq!(store.agent_policy().unwrap(), AgentPolicy::default());
    }

    #[test]
    fn switching_out_leaves_existing_members_where_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("refactor", "/work/app").unwrap();
        store
            .join(&group.id, member(AgentKind::CODEX, "x1"))
            .unwrap();
        store.set_agent_allowed(AgentKind::CODEX, false).unwrap();
        assert_eq!(store.load(&group.id).unwrap().members.len(), 1);
    }

    #[test]
    fn a_damaged_file_refuses_joins_instead_of_reading_as_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let group = store.create("refactor", "/work/app").unwrap();
        std::fs::write(dir.path().join(AGENTS_FILE), "{ not json").unwrap();

        assert!(store.agent_policy().is_err());
        assert!(store
            .join(&group.id, member(AgentKind::REBON, "r1"))
            .is_err());
        assert!(store.set_agent_allowed(AgentKind::REBON, false).is_err());
    }
}
