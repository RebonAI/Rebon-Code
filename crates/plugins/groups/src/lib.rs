//! `groups`: the feature plugin that makes a Rebon session a member of an
//! agent group (RFC-0009 §16).
//!
//! Two seats, one store:
//!
//! - the **tool seat** gets the seven group tools (`group_info`,
//!   `group_join`, …), `rebon-group`'s own, run as this session;
//! - the **attachment seat** gets `group_inbox` ([`inbox`]) at
//!   [`Order::Mailbox`]: what the group wrote to the session, delivered
//!   between rounds.
//!
//! The same tools reach other agents through `rebon mcp serve`, which a
//! project's `.mcp.json` starts for Rebon sessions too. That server leaves
//! its group tools out when the session calling is Rebon's, because this
//! plugin already gave them to it; turning the plugin off
//! (`plugins.groups.enabled: false`) takes the session out of groups.

use std::sync::Arc;

use rebon_core::attachment_seat::{AttachmentSeatService, Order, ATTACHMENT_SEAT_SERVICE};
use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_group::GroupStore;
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod inbox;
pub mod tools;

/// Stable id: the config key `plugins.groups.enabled`.
pub const PLUGIN_ID: &str = "groups";

const TOOLS_PROVIDER_ID: &str = "groups";
const INBOX_PROVIDER_ID: &str = "group-inbox";

pub struct GroupsPlugin {
    store: GroupStore,
}

impl GroupsPlugin {
    pub fn new(store: GroupStore) -> Self {
        Self { store }
    }
}

impl Plugin for GroupsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID).inject(&[TOOL_SEAT_SERVICE, ATTACHMENT_SEAT_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        let tools: Vec<Arc<dyn rebon_tool::Tool>> = tools::tools(&self.store)
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn rebon_tool::Tool>)
            .collect();
        seat.register_tools(ctx, TOOLS_PROVIDER_ID, Priority::Feature, tools)?;

        let attachments = ctx.require::<AttachmentSeatService>()?;
        attachments.register(
            ctx,
            INBOX_PROVIDER_ID,
            Order::Mailbox,
            Arc::new(inbox::GroupInboxProducer::new(self.store.clone())),
        )
    }
}

fn make(host: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(GroupsPlugin::new(GroupStore::new(
        rebon_group::default_root(&host.config_dir),
    ))))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Agent groups (group_* tools, group inbox)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::attachment_seat::AttachmentSeat;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{Tool, ToolContext, ToolResolver};
    use serde_json::json;

    /// Stands in for `core-tools`: the two root seats this plugin needs.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[TOOL_SEAT_SERVICE, ATTACHMENT_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<ToolSeatService>(ToolSeat::new())?;
            ctx.provide::<AttachmentSeatService>(AttachmentSeat::new())
        }
    }

    fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(SeatPlugin))
    }

    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "test-seat",
            title: "Test seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_seat,
        },
        PLUGIN,
    ];

    #[test]
    fn the_switch_puts_the_group_tools_on_the_seat_and_takes_them_off() {
        let config = tempfile::tempdir().unwrap();
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: config.path().to_path_buf(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let seat = kernel.context().get::<ToolSeatService>().unwrap();
        for name in [
            rebon_group::tools::GROUP_JOIN,
            rebon_group::tools::GROUP_SEND,
            rebon_group::tools::GROUP_INBOX,
        ] {
            assert!(seat.resolve(name, None).unwrap().is_some(), "{name}");
        }
        registry.set_enabled(PLUGIN_ID, false).unwrap();
        assert!(seat
            .resolve(rebon_group::tools::GROUP_JOIN, None)
            .unwrap()
            .is_none());
    }

    /// Run as the session: no agent, no session id to pass.
    #[tokio::test]
    async fn a_session_joins_and_writes_as_itself() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());
        let tools = tools::tools(&store);
        let tool = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.id().as_str() == name)
                .unwrap()
        };
        let context = ToolContext::default()
            .with_session_id("k7m2q-4xr9t")
            .with_cwd("/work/app");
        let joined = tool(rebon_group::tools::GROUP_JOIN)
            .call(json!({ "group": "refactor", "alias": "planner" }), &context)
            .await
            .unwrap();
        assert_eq!(joined["you"], "planner");
        let group = store.find("/work/app", "refactor").unwrap().unwrap();
        assert_eq!(group.members[0].agent, "rebon");
        assert_eq!(group.members[0].session_id, "k7m2q-4xr9t");

        let unknown = ToolContext::default().with_cwd("/work/app");
        assert!(tool(rebon_group::tools::GROUP_INFO)
            .call(json!({}), &unknown)
            .await
            .is_err());
    }
}
