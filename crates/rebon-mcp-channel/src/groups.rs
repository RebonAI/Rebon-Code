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
