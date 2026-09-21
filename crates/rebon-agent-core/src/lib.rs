//! The agent turn seam, in the one place both directions can reach it.
//!
//! Rebon runs agents two ways. In the **local** leg it *is* the agent:
//! the local engine drives a model client, dispatches tools, and writes
//! the transcript. In the **delegated** leg it hands the turn to somebody
//! else's agent CLI over stdio and relays what comes back. An editor
//! driving Rebon over ACP is a third arrangement again.
//!
//! All three need the same three things, and none of them belongs to the
//! ACP server:
//!
//! - [`prompt_executor`] — "run one turn and tell me why it stopped".
//! - [`publisher`] — where a running turn sends streaming updates and
//!   permission requests, without knowing who is listening.
//! - [`backend`] — which agent runs the turn at all, and what it can
//!   and cannot do.
//!
//! [`turn_gate`] sits on the first of those: it decorates a
//! [`prompt_executor::PromptExecutor`] to decide whether a turn the
//! model ended on its own is actually finished. It lives here for the
//! same reason the trait does — every leg ends a turn through that one
//! seam, so gating it in one place covers all of them.
//!
//! The three live on their own crate so an ACP client does not have to depend
//! on an ACP server to speak the same turn vocabulary.
//!
//! Nothing here is re-exported through the ACP server: callers import from
//! the crate that actually owns the type.
//!
//! [`routing`] owns session selection and reuse over this same turn seam;
//! [`registry`] holds lazy backend factories supplied by the host.
//! [`file_history`] and [`handoff`] own the shared write boundary and pure
//! conversation projection. Concrete config and engine assembly belongs to
//! the host, keeping those consumers out of this crate's dependency graph.
//! [`declared_source`] is the kernel seam the rest of the declarations arrive
//! over: a plugin that knows about agent CLIs this crate cannot see — a remote
//! host behind `ssh`, say — registers there, and the front end collects them
//! without depending on it.
//!
//! [`model_router`] answers the other question a turn needs settled before it
//! starts: which provider, model and reasoning effort it runs on. Both the
//! sub-agent spawner and the workflow runner ask it, and neither may depend on
//! the other, so the precedence is written once here.

pub mod backend;
pub mod declared_source;
pub mod file_history;
pub mod handoff;
pub mod model_router;
pub mod prompt_executor;
pub mod publisher;
pub mod registry;
pub mod routing;
pub mod turn_gate;

pub use backend::{
    AgentBackend, AgentBackendError, AgentBackendKind, AgentBackendSwitch, AgentCapabilities,
    AgentSessionSpec, AgentSessionStart, BackendPromptExecutor, LocalAgentBackend,
    SessionResumeMode, SteerOutcome,
};
pub use prompt_executor::{
    DenialReplayRequest, PromptCancel, PromptExecutor, PromptExecutorError, PromptOutcome,
    PromptRequest, SkillInvocationRequest, StubPromptExecutor,
};
pub use publisher::{
    ChannelPermissionRequestPublisher, ChannelSessionUpdatePublisher, MemorySessionUpdatePublisher,
    SessionUpdatePublisher,
};
pub use turn_gate::{TurnCompletionGate, TurnGatePolicy, TurnProgressProbe, DEFAULT_AUDIT_PROMPT};
