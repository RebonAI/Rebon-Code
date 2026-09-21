//! `remote`: running a session against a project on another machine.
//!
//! Rebon already speaks both halves of this, so the plugin is small and the
//! transport is most of it. `rebon --acp` is an ACP server on stdio and
//! `rebon-acp-client` already knows how to start an agent CLI; a remote host
//! is therefore an agent CLI whose command happens to begin with `ssh`. The
//! split below is drawn where the reasons to change differ:
//!
//! * **[`remote`]** — the transport. ssh argv and multiplexing, the
//!   `remotes.json` store, remote platform detection, the probe and install
//!   scripts.
//! * **[`agents`]** — a host as a
//!   [`rebon_agent_core::routing::DeclaredAgent`], and the seam that hands
//!   those to whoever is assembling a session.
//! * **[`cli`]** — `rebon remote …`.
//!
//! # The switch
//!
//! `plugins.remote.enabled = false` disposes this context, and with it the
//! `declared-agent-source` seat. Nothing then declares a `remote:` agent, so
//! `/agent` and the agent list offer none, `/context` lists none, and
//! `--remote <name>` fails with "the remote plugin is disabled" rather than
//! "no such host" — the two are different problems and the person reading
//! the message has to be able to tell them apart.
//!
//! What stays is `rebon remote …`. Those are clap subcommands parsed before
//! a kernel exists — see [`cli`] — and a process that has not booted a
//! plugin registry cannot consult a plugin switch. Being able to read and
//! edit the host list while the feature that uses it is off is the right
//! way round anyway.
//!
//! # What is deliberately not here
//!
//! `--remote` itself. Which host a run demands is a flag on the binary,
//! parsed with every other flag before anything is loaded; this crate turns
//! the name into an agent id and declares the hosts, and the front end fails
//! the session when the demand cannot be met.

use std::sync::Arc;

use rebon_agent_core::declared_source::DeclaredAgentSourceService;
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};

pub mod agents;
pub mod cli;
pub mod remote;

#[cfg(test)]
mod test_home;

pub use agents::{server_version, RemoteAgents};
pub use remote::*;

/// Stable id: the config key `plugins.remote.enabled` and the name in
/// `/kernel plugins`.
pub const PLUGIN_ID: &str = "remote";

pub struct RemotePlugin;

impl Plugin for RemotePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        // The seat goes on this plugin's own context, so disabling the
        // plugin takes it out of the registry. A front end assembling a
        // session then finds no source, declares no remotes, and refuses
        // `--remote` — which is the whole of "this feature is off".
        ctx.provide::<DeclaredAgentSourceService>(Arc::new(RemoteAgents))?;
        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(RemotePlugin))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Remote hosts over ssh (--remote, rebon remote)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{RemoteHost, RemoteStore};
    use crate::test_home::TestConfigHome;
    use rebon_agent_core::declared_source::{declared_agents, demanded_agent_id};
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};

    static DEFS: &[PluginDef] = &[PLUGIN];

    fn booted() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    fn store_with_one_host(home: &TestConfigHome) {
        let mut store = RemoteStore::default();
        store
            .insert(
                RemoteHost::from_target("prod", "deploy@build.example").unwrap(),
                false,
            )
            .unwrap();
        store.save(home.path()).unwrap();
    }

    /// A configured host reaches `/agent`, the agent list and `/context`
    /// through the seat — and leaves all three when the plugin is switched
    /// off, without the store being touched.
    #[test]
    fn the_switch_takes_the_configured_hosts_out_of_the_agent_list_and_puts_them_back() {
        let home = TestConfigHome::new();
        store_with_one_host(&home);
        let (kernel, registry) = booted();

        let declared = declared_agents(kernel.context(), None, None);
        assert_eq!(
            declared.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["remote:prod"]
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("remote is a feature plugin");
        assert!(
            declared_agents(kernel.context(), None, None).is_empty(),
            "a disabled plugin declares no hosts, however many are configured"
        );

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(declared_agents(kernel.context(), None, None).len(), 1);
    }

    /// And the demand is refused rather than mis-answered: with the plugin
    /// off nothing can name a host, which is what lets the front end say
    /// "the feature is disabled" instead of "no such host".
    #[test]
    fn the_switch_takes_away_the_ability_to_name_a_demanded_host() {
        let _home = TestConfigHome::new();
        let (kernel, registry) = booted();

        assert_eq!(
            demanded_agent_id(kernel.context(), "prod"),
            Some("remote:prod".to_string())
        );

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("remote is a feature plugin");
        assert_eq!(demanded_agent_id(kernel.context(), "prod"), None);

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert_eq!(
            demanded_agent_id(kernel.context(), "prod"),
            Some("remote:prod".to_string())
        );
    }

    /// Loading the plugin reads nothing: the store is opened when a session
    /// asks for declarations, so a `rebon remote add` in another terminal is
    /// visible to the next session without this process restarting.
    #[test]
    fn a_host_added_after_the_plugin_loaded_is_still_declared() {
        let home = TestConfigHome::new();
        let (kernel, _registry) = booted();
        assert!(declared_agents(kernel.context(), None, None).is_empty());

        store_with_one_host(&home);
        assert_eq!(declared_agents(kernel.context(), None, None).len(), 1);
    }
}
