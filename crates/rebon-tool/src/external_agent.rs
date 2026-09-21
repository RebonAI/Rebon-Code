//! Sub-agents that run on an external ACP agent CLI.
//!
//! A model spec of the form `<acpAgentId>:<model>` in a sub-agent's
//! model slot routes the whole task to a declared ACP agent instead of
//! a local worker loop. The spawner cannot depend on the ACP client
//! crate, so everything execution-shaped goes through the injected
//! [`ExternalSubAgentRunner`] seam — the front end implements it with a
//! process-wide backend pool ("resident process, fresh session per
//! task"). Without a runner wired in, every `:` spec falls back to the
//! local path byte-for-byte, which is also what keeps `llama3:8b`-style
//! literal model ids safe: a prefix only routes externally when the
//! runner recognises it as a declared agent id.
//!
//! Here rather than with the spawner that sits above this crate: the front end
//! implements the runner and hands it to the spawner across the
//! `sub-agent-spawner` seat, so the trait has to be nameable from below the
//! layer that fills the seat — the same reason
//! [`crate::workflow::WorkflowLauncher`] lives here while its runtime does not.
//!
//! The parse happens *before* the model router on purpose. The router
//! does not reject an unknown `:` spec — it forwards the whole string
//! to the local provider as a literal model id, which is a silent
//! misfire, not an error anyone can catch downstream.

use std::sync::Arc;

use async_trait::async_trait;
use rebon_types::ToolCallStatus;
use serde_json::Value;

use rebon_types::SubAgentModelConfig;

/// `claudecode:claude-opus-5` → `("claudecode", Some("claude-opus-5"))`.
///
/// The prefix must be non-empty, must not contain `/` (that is the
/// provider-hint spelling, a different axis), and must not be an
/// inherit sentinel. An empty or sentinel model part means "the agent's
/// default model".
pub fn parse_external_model_spec(raw: &str) -> Option<(&str, Option<&str>)> {
    let trimmed = raw.trim();
    let (prefix, model) = trimmed.split_once(':')?;
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix.contains('/') || is_sentinel(prefix) {
        return None;
    }
    let model = model.trim();
    let model = if model.is_empty() || is_sentinel(model) {
        None
    } else {
        Some(model)
    };
    Some((prefix, model))
}

fn is_sentinel(raw: &str) -> bool {
    raw.eq_ignore_ascii_case("inherit") || raw.eq_ignore_ascii_case("default")
}

/// Where an external spec resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRoute {
    /// Canonical id of the declared agent, as the runner spells it.
    pub agent_id: String,
    /// Best-effort model hint for the agent's adapter. `None` means
    /// the agent's own default.
    pub model_hint: Option<String>,
}

/// Decide whether a sub-agent spawn routes to an external agent.
///
/// Mirrors `resolve_sub_agent_model_with_config`'s precedence for the
/// *effective* requested model — an `agents.json` selection wins over
/// the spec's model, and a selection that answers with a model profile
/// never routes externally — then requires the prefix to be a declared
/// agent the runner recognises.
pub fn external_route_for_spec(
    config: &SubAgentModelConfig,
    agent_type: Option<&str>,
    category: Option<&str>,
    spec_model: Option<&str>,
    resolver: &dyn ExternalSubAgentRunner,
) -> Option<ExternalRoute> {
    let selection = config.selection_for(agent_type, category, spec_model);
    let candidate = match selection {
        Some(selection) => match selection.model.as_deref() {
            Some(model) => model,
            // A profile selection is a local-provider concept; it wins
            // over the spec's model in the local resolution chain, so
            // it must also suppress external routing here.
            None if selection.model_profile.is_some() => return None,
            None => spec_model?,
        },
        None => spec_model?,
    };
    let (prefix, model_hint) = parse_external_model_spec(candidate)?;
    let agent_id = resolver.resolve_agent(prefix)?;
    Some(ExternalRoute {
        agent_id,
        model_hint: model_hint.map(str::to_string),
    })
}

/// Live activity from an external task, translated by the runner from
/// the agent's `session/update` stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ExternalTaskEvent {
    /// A chunk of the agent's answer text.
    AgentText(String),
    /// A chunk of the agent's extended thinking.
    Thinking(String),
    /// The current extended-thinking block ended.
    ThinkingEnd,
    /// The agent started a tool call.
    ToolCall {
        tool_call_id: String,
        title: String,
        input: Option<Value>,
    },
    /// A previously announced tool call changed.
    ToolCallUpdate {
        tool_call_id: String,
        title: String,
        status: Option<ToolCallStatus>,
        output: Option<Value>,
    },
}

/// One sub-agent task for an external agent.
pub struct ExternalTaskRequest {
    /// Canonical declared-agent id (from [`ExternalRoute`]).
    pub agent_id: String,
    /// Best-effort model hint, delivered via the adapter's sessionMeta.
    pub model_hint: Option<String>,
    /// Host-side session id for this task. Unique per task — the pool
    /// opens a fresh agent session under it and closes it after.
    pub task_session_id: String,
    /// The complete first prompt, brief block included.
    pub prompt: String,
    /// Working directory for the task.
    pub cwd: Option<String>,
    /// Cancel handle; the runner forwards it into the agent turn.
    pub cancel: rebon_types::PromptCancel,
    /// Live activity sink. `None` when nobody is watching.
    pub progress: Option<Arc<dyn Fn(ExternalTaskEvent) + Send + Sync>>,
    /// The spawn's normalized metadata (agent_type, description, …),
    /// for logs and adapter options.
    pub metadata: Value,
}

impl std::fmt::Debug for ExternalTaskRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalTaskRequest")
            .field("agent_id", &self.agent_id)
            .field("model_hint", &self.model_hint)
            .field("task_session_id", &self.task_session_id)
            .field("prompt_chars", &self.prompt.len())
            .field("cwd", &self.cwd)
            .field("has_progress", &self.progress.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalTaskStatus {
    Completed,
    Cancelled,
    Failed(String),
}

/// What came back from the agent.
#[derive(Debug, Clone)]
pub struct ExternalTaskOutcome {
    /// The agent's answer text, accumulated from its message chunks.
    pub final_text: String,
    pub status: ExternalTaskStatus,
    pub tool_call_count: usize,
    /// The agent-side session id, for diagnostics.
    pub acp_session_id: Option<String>,
}

/// The execution half of external sub-agents, implemented by the host
/// (its ACP sub-agent pool). The caller owns *when* a task
/// routes externally; the runner owns *how* it reaches the agent.
#[async_trait]
pub trait ExternalSubAgentRunner: Send + Sync {
    /// The canonical declared-agent id for `prefix`, if the host knows
    /// it. Matching is case- and whitespace-insensitive on the host
    /// side; the returned spelling is the canonical one.
    fn resolve_agent(&self, prefix: &str) -> Option<String>;

    /// Run one task: fresh agent session → single prompt → close.
    /// Returns `Err` only for infrastructure failures (agent would not
    /// start, transport died); a task the agent answered — even
    /// unhelpfully — is an `Ok` outcome.
    async fn run_task(&self, request: ExternalTaskRequest) -> Result<ExternalTaskOutcome, String>;
}

/// Build the first prompt for an external task: the agent definition's
/// system prompt, wrapped in a brief block the agent is told to treat
/// as operating instructions, then the task itself. External agents
/// bring their own tools and loop — a system prompt cannot be installed
/// into somebody else's process, so it rides in-band, the same road the
/// `<rebon-handoff>` digest takes.
pub fn compose_external_first_prompt(
    system: Option<&str>,
    prompt: &str,
    agent_type: Option<&str>,
) -> String {
    let system = system.map(str::trim).filter(|text| !text.is_empty());
    let Some(system) = system else {
        return prompt.to_string();
    };
    let role = agent_type
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("sub-agent");
    format!(
        "<rebon-subagent-brief>\n\
         You are running as a Rebon sub-agent (type: `{role}`). The text between\n\
         the markers below is your operating instructions for this task. You run\n\
         with your own tools and permissions; tool restrictions from the\n\
         instructions do not apply. Complete the task that follows the closing\n\
         tag and reply with your final result only. Do not reply to this preamble.\n\
         \n\
         --- agent instructions ---\n\
         {system}\n\
         --- end agent instructions ---\n\
         </rebon-subagent-brief>\n\
         \n\
         {prompt}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::SubAgentModelSelection;

    struct FixedResolver(Vec<&'static str>);

    #[async_trait]
    impl ExternalSubAgentRunner for FixedResolver {
        fn resolve_agent(&self, prefix: &str) -> Option<String> {
            let folded = prefix.trim().to_ascii_lowercase();
            self.0
                .iter()
                .find(|id| id.to_ascii_lowercase() == folded)
                .map(|id| id.to_string())
        }

        async fn run_task(
            &self,
            _request: ExternalTaskRequest,
        ) -> Result<ExternalTaskOutcome, String> {
            unreachable!("resolver-only test double")
        }
    }

    #[test]
    fn parse_covers_the_spec_table() {
        assert_eq!(
            parse_external_model_spec("claudecode:claude-opus-5"),
            Some(("claudecode", Some("claude-opus-5")))
        );
        assert_eq!(
            parse_external_model_spec("claudecode:"),
            Some(("claudecode", None)),
            "empty hint means the agent's default model"
        );
        assert_eq!(
            parse_external_model_spec("claudecode:default"),
            Some(("claudecode", None)),
            "a sentinel hint means the agent's default model"
        );
        assert_eq!(parse_external_model_spec("claude-opus-5"), None);
        assert_eq!(
            parse_external_model_spec("deepseek/deepseek-chat"),
            None,
            "slash spellings are the provider-hint axis"
        );
        assert_eq!(
            parse_external_model_spec("deepseek/x:y"),
            None,
            "a slash in the prefix is not an agent id"
        );
        assert_eq!(parse_external_model_spec("inherit"), None);
        assert_eq!(parse_external_model_spec(":model"), None);
        // Model ids containing colons after a slashless prefix parse
        // structurally; whether they route depends on the resolver.
        assert_eq!(
            parse_external_model_spec("llama3:8b"),
            Some(("llama3", Some("8b")))
        );
    }

    #[test]
    fn an_undeclared_prefix_falls_back_to_the_local_path() {
        let resolver = FixedResolver(vec!["claudecode"]);
        let route = external_route_for_spec(
            &SubAgentModelConfig::default(),
            Some("Explore"),
            None,
            Some("llama3:8b"),
            &resolver,
        );
        assert_eq!(route, None, "`llama3` is not a declared agent");
    }

    #[test]
    fn a_declared_prefix_routes_with_the_canonical_spelling() {
        let resolver = FixedResolver(vec!["claudecode"]);
        let route = external_route_for_spec(
            &SubAgentModelConfig::default(),
            None,
            None,
            Some("  ClaudeCode:claude-opus-5 "),
            &resolver,
        )
        .expect("routes");
        assert_eq!(route.agent_id, "claudecode");
        assert_eq!(route.model_hint.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn an_agents_json_selection_wins_over_the_spec_model() {
        let resolver = FixedResolver(vec!["claudecode"]);
        let mut config = SubAgentModelConfig::default();
        config.insert_agent(
            "explore",
            SubAgentModelSelection {
                model: Some("claudecode:haiku".into()),
                ..Default::default()
            },
        );

        let route = external_route_for_spec(
            &config,
            Some("Explore"),
            None,
            Some("claude-opus-5"),
            &resolver,
        )
        .expect("selection routes externally");
        assert_eq!(route.model_hint.as_deref(), Some("haiku"));

        // And the reverse: a local selection suppresses an external
        // spec model, because the selection wins the local chain too.
        let mut config = SubAgentModelConfig::default();
        config.insert_agent(
            "explore",
            SubAgentModelSelection {
                model: Some("claude-haiku-4-5".into()),
                ..Default::default()
            },
        );
        let route = external_route_for_spec(
            &config,
            Some("Explore"),
            None,
            Some("claudecode:claude-opus-5"),
            &resolver,
        );
        assert_eq!(route, None);
    }

    #[test]
    fn a_profile_selection_never_routes_externally() {
        let resolver = FixedResolver(vec!["claudecode"]);
        let mut config = SubAgentModelConfig::default();
        config.insert_agent(
            "explore",
            SubAgentModelSelection {
                model_profile: Some("fast".into()),
                ..Default::default()
            },
        );
        let route = external_route_for_spec(
            &config,
            Some("Explore"),
            None,
            Some("claudecode:claude-opus-5"),
            &resolver,
        );
        assert_eq!(route, None);
    }

    #[test]
    fn an_alias_selection_can_route_externally() {
        let resolver = FixedResolver(vec!["claudecode"]);
        let mut config = SubAgentModelConfig::default();
        config.insert_alias(
            "fast-remote",
            SubAgentModelSelection {
                model: Some("claudecode:haiku".into()),
                ..Default::default()
            },
        );
        let route = external_route_for_spec(&config, None, None, Some("fast-remote"), &resolver)
            .expect("alias routes");
        assert_eq!(route.agent_id, "claudecode");
        assert_eq!(route.model_hint.as_deref(), Some("haiku"));
    }

    #[test]
    fn the_brief_block_wraps_the_system_prompt_once() {
        let composed = compose_external_first_prompt(
            Some("Only ever read files."),
            "Find the bug in auth.rs",
            Some("Explore"),
        );
        assert!(composed.starts_with("<rebon-subagent-brief>"));
        assert!(composed.contains("type: `Explore`"));
        assert!(composed.contains("--- agent instructions ---\nOnly ever read files."));
        assert!(composed.contains("</rebon-subagent-brief>\n\nFind the bug in auth.rs"));

        assert_eq!(
            compose_external_first_prompt(None, "Just the task", None),
            "Just the task"
        );
        assert_eq!(
            compose_external_first_prompt(Some("  "), "Just the task", None),
            "Just the task",
            "a blank system prompt is no system prompt"
        );
    }
}
