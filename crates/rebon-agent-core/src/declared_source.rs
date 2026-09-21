//! Agent declarations this crate cannot see, as a seam.
//!
//! [`crate::routing::DeclaredAgent`] is how "an agent CLI and how to start
//! it" is described once for every surface that declares one. Two of those
//! surfaces are configuration the host reads directly — `acpAgents` in
//! `config.json` and `runtime: acp` frontmatter — and the host's agent
//! assembly normalises both. A third is not: a host in `remotes.json` is an
//! agent CLI with `ssh` in front of it, and everything that knows how to
//! build that command line (the argv, the connection multiplexing, the
//! server path on the far end) belongs to a plugin, which sits *above* this
//! crate.
//!
//! So the front end does not ask the plugin. It asks the kernel for whoever
//! answers here, and gets a list. With no provider the list is empty, which
//! is exactly what a session gets today when `remotes.json` does not exist:
//! the local engine, the configured agents, and no remotes.
//!
//! # Why the demand is a second question
//!
//! `--remote <name>` is a demand, not a preference — a session that cannot
//! reach the named host must fail rather than quietly run the user's prompts
//! against the one filesystem they ruled out. Resolving the name to an agent
//! id therefore cannot go through "look it up in the list and see": an
//! unreachable host is still *declared*, while a disabled plugin declares
//! nothing at all, and those two must not produce the same message.
//! [`demanded_agent_id`] answers the second question — "is there anyone here
//! who could name this host?" — and `None` means the feature is off, not that
//! the host is missing.

use rebon_kernel::Service;

use crate::routing::DeclaredAgent;

/// JSON/typed name of the declared-agent seam.
pub const DECLARED_AGENT_SOURCE_SERVICE: &str = "declared-agent-source";

/// The provider behind the seam.
///
/// One provider, not a chain: the ids a source hands out are namespaced by
/// its own prefix (`remote:` today), and a second source would have to pick
/// another. Adding one is a change here, deliberately, rather than a
/// registration that silently starts competing for names.
pub trait DeclaredAgentSource: Send + Sync {
    /// Every agent this source declares, in the order it wants them listed.
    ///
    /// `demanded` is the name the user asked for on the command line and
    /// `workspace_path` the project-directory override that applies to *that*
    /// one and to no other — a later `/agent` switch to a different host in
    /// the same session still lands in that host's own directory.
    fn declared_agents(
        &self,
        demanded: Option<&str>,
        workspace_path: Option<&str>,
    ) -> Vec<DeclaredAgent>;

    /// The agent id `demanded` names, whether or not the host exists.
    ///
    /// A name that was never configured still has an id; the switch that
    /// follows is what discovers there is no such agent, and it says so in
    /// terms of the host. Answering only for configured hosts here would
    /// turn "this host is unreachable" into "this feature is off".
    fn agent_id_for(&self, demanded: &str) -> String;
}

/// Typed definition for the kernel's `declared-agent-source` seat.
pub struct DeclaredAgentSourceService;

impl Service for DeclaredAgentSourceService {
    type Interface = dyn DeclaredAgentSource;
    const NAME: &'static str = DECLARED_AGENT_SOURCE_SERVICE;
}

/// Every externally-declared agent the kernel scope `ctx` can see.
///
/// Empty with no provider: the session opens on the local engine and the
/// agents `config.json` declared, and nothing mentions remotes.
pub fn declared_agents(
    ctx: &rebon_kernel::Context,
    demanded: Option<&str>,
    workspace_path: Option<&str>,
) -> Vec<DeclaredAgent> {
    match ctx.get::<DeclaredAgentSourceService>() {
        Some(source) => source.declared_agents(demanded, workspace_path),
        None => Vec::new(),
    }
}

/// The agent id a demanded host name resolves to, or `None` when nothing on
/// this kernel can name one.
///
/// `None` is the answer a caller must not paper over: the user named a
/// machine, and the feature that reaches machines is not loaded.
pub fn demanded_agent_id(ctx: &rebon_kernel::Context, demanded: &str) -> Option<String> {
    Some(
        ctx.get::<DeclaredAgentSourceService>()?
            .agent_id_for(demanded),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::AgentOrigin;
    use rebon_kernel::Kernel;
    use std::sync::Arc;

    struct OneHost;

    fn declared(id: &str, workspace_cwd: Option<&str>) -> DeclaredAgent {
        DeclaredAgent {
            id: id.to_string(),
            label: id.to_string(),
            command: "ssh".to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            origin: AgentOrigin::Remote,
            inject_fs_tools: false,
            session_meta: None,
            install_hint: None,
            workspace_cwd: workspace_cwd.map(str::to_string),
        }
    }

    impl DeclaredAgentSource for OneHost {
        fn declared_agents(
            &self,
            demanded: Option<&str>,
            workspace_path: Option<&str>,
        ) -> Vec<DeclaredAgent> {
            let path = (demanded == Some("prod"))
                .then_some(workspace_path)
                .flatten();
            vec![declared("remote:prod", path.or(Some("/srv/app")))]
        }

        fn agent_id_for(&self, demanded: &str) -> String {
            format!("remote:{demanded}")
        }
    }

    /// No provider, no declarations — and no error. A session with the
    /// feature switched off is a session with no remotes in it, which is the
    /// same session someone who never configured one already has.
    #[test]
    fn a_kernel_without_a_source_declares_nothing() {
        let kernel = Kernel::new();
        assert!(declared_agents(kernel.context(), None, None).is_empty());
    }

    /// And the demand is refusable rather than guessable: with nobody to name
    /// the host, the caller is told there is no answer instead of being handed
    /// an id that no backend will ever match.
    #[test]
    fn a_kernel_without_a_source_cannot_name_a_demanded_host() {
        let kernel = Kernel::new();
        assert_eq!(demanded_agent_id(kernel.context(), "prod"), None);
    }

    /// A provider answers both questions, and stops answering when its own
    /// scope goes away — which is what `plugins.<id>.enabled = false` does.
    #[test]
    fn a_provider_answers_until_its_scope_is_disposed() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("remote");
        ctx.provide::<DeclaredAgentSourceService>(Arc::new(OneHost))
            .unwrap();

        let agents = declared_agents(kernel.context(), None, None);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "remote:prod");
        assert_eq!(agents[0].origin, AgentOrigin::Remote);
        assert_eq!(
            demanded_agent_id(kernel.context(), "prod"),
            Some("remote:prod".to_string())
        );

        ctx.dispose();
        assert!(declared_agents(kernel.context(), None, None).is_empty());
        assert_eq!(demanded_agent_id(kernel.context(), "prod"), None);
    }

    /// The path override reaches the host the user opened and no other.
    #[test]
    fn the_workspace_path_override_follows_the_demanded_host() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("remote");
        ctx.provide::<DeclaredAgentSourceService>(Arc::new(OneHost))
            .unwrap();

        let opened = declared_agents(kernel.context(), Some("prod"), Some("/srv/other"));
        assert_eq!(opened[0].workspace_cwd.as_deref(), Some("/srv/other"));

        let other = declared_agents(kernel.context(), Some("staging"), Some("/srv/other"));
        assert_eq!(other[0].workspace_cwd.as_deref(), Some("/srv/app"));
    }

    /// An id exists for a host that does not: naming is not existence, and
    /// the switch that follows is what reports the missing host.
    #[test]
    fn a_source_names_a_host_it_has_never_heard_of() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("remote");
        ctx.provide::<DeclaredAgentSourceService>(Arc::new(OneHost))
            .unwrap();

        assert_eq!(
            demanded_agent_id(kernel.context(), "never-configured"),
            Some("remote:never-configured".to_string())
        );
    }
}
