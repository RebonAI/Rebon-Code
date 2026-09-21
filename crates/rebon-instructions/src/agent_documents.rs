//! Per-agent memory, as a seam.
//!
//! An agent definition may declare `memory: user | project | local`, and a
//! sub-agent spawned from it gets that memory appended to its system prompt.
//! Two plugins are involved and neither may depend on the other: `agents`
//! spawns the sub-agent and knows the declared scope, `memory` owns the store
//! and the prompt it renders. So the shape of the answer lives here, below
//! both, exactly as [`crate::loaded_documents`] does for `/memory`.
//!
//! Fail-open is not an option and fail-closed is cheap: with no provider the
//! sub-agent is spawned without agent memory, which is what an agent whose
//! declared scope fails to parse already gets.

use rebon_kernel::Service;

/// JSON/typed name of the agent-memory seam.
pub const AGENT_MEMORY_SERVICE: &str = "agent-memory-prompt";

/// The provider behind the seam.
pub trait AgentMemoryPrompt: Send + Sync {
    /// The memory prompt for `agent_type` at `scope`, rooted at `cwd`, or
    /// `None` when `scope` is not one this provider recognises.
    ///
    /// `scope` arrives as the raw frontmatter string rather than a parsed
    /// enum: the vocabulary belongs to whoever stores the memory, and a
    /// value this provider does not know is the same "no agent memory"
    /// answer as no provider at all.
    fn agent_memory_prompt(
        &self,
        agent_type: &str,
        scope: &str,
        cwd: &std::path::Path,
    ) -> Option<String>;
}

/// Typed definition for the kernel's `agent-memory-prompt` seat.
pub struct AgentMemoryPromptService;

impl Service for AgentMemoryPromptService {
    type Interface = dyn AgentMemoryPrompt;
    const NAME: &'static str = AGENT_MEMORY_SERVICE;
}

/// The agent-memory prompt for this scope, when the kernel scope `ctx` can
/// see a provider. `None` means no agent memory for this spawn.
pub fn agent_memory_prompt(
    ctx: &rebon_kernel::Context,
    agent_type: &str,
    scope: &str,
    cwd: &std::path::Path,
) -> Option<String> {
    ctx.get::<AgentMemoryPromptService>()?
        .agent_memory_prompt(agent_type, scope, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;
    use std::path::Path;
    use std::sync::Arc;

    struct Fixed;

    impl AgentMemoryPrompt for Fixed {
        fn agent_memory_prompt(
            &self,
            agent_type: &str,
            scope: &str,
            _cwd: &Path,
        ) -> Option<String> {
            (scope == "project").then(|| format!("memory for {agent_type}"))
        }
    }

    /// No provider, no agent memory — and the caller still gets an answer.
    #[test]
    fn a_scope_without_a_provider_answers_none() {
        let kernel = Kernel::new();
        assert_eq!(
            agent_memory_prompt(kernel.context(), "coder", "project", Path::new("/repo")),
            None
        );
    }

    /// A provider answers per scope, and leaves the seam when its own scope
    /// is disposed.
    #[test]
    fn a_provider_answers_until_its_scope_is_disposed() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("memory");
        ctx.provide::<AgentMemoryPromptService>(Arc::new(Fixed))
            .unwrap();

        assert_eq!(
            agent_memory_prompt(kernel.context(), "coder", "project", Path::new("/repo")),
            Some("memory for coder".to_string())
        );
        assert_eq!(
            agent_memory_prompt(kernel.context(), "coder", "nonsense", Path::new("/repo")),
            None,
            "an unknown scope is the same answer as no provider"
        );

        ctx.dispose();
        assert_eq!(
            agent_memory_prompt(kernel.context(), "coder", "project", Path::new("/repo")),
            None
        );
    }
}
