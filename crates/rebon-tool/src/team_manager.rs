use std::sync::Arc;

use async_trait::async_trait;

use crate::PermissionBroker;
use rebon_tools_core::ToolErrorPresentation;

/// Team state that [`ToolContext`](crate::ToolContext) carries in its
/// extension bag.
///
/// Storage only — read and written through the unchanged `team_manager()` /
/// `team_identity()` accessors.
#[derive(Clone, Default)]
pub struct TeamContext {
    pub manager: Option<Arc<dyn TeamManager>>,
    pub identity: Option<crate::TeamIdentityContext>,
}

/// Request to spawn a teammate into the current team.
#[derive(Clone)]
pub struct TeammateSpawnSpec {
    /// Team name the teammate should join.
    pub team_name: String,
    /// Human-readable teammate name.
    pub name: String,
    /// Initial prompt for the teammate.
    pub prompt: String,
    /// Optional display/agent type label.
    pub agent_type: Option<String>,
    /// Optional model override.
    pub model: Option<String>,
    /// Optional model profile override.
    pub model_profile: Option<String>,
    /// Optional provider override.
    pub provider: Option<String>,
    /// Optional spawn mode (`plan`, `default`, ...).
    pub mode: Option<String>,
    /// Optional reasoning effort override (`xhigh`, `high`, `medium`, `low`).
    pub effort: Option<String>,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Optional system prompt assembled by the parent Agent tool.
    pub system: Option<String>,
    /// Working directory inherited from the parent tool context. The
    /// teammate worker runs here instead of the process launch
    /// directory.
    pub cwd: Option<String>,
    /// Additional authorized directories inherited from the parent
    /// tool context.
    pub additional_working_directories: Vec<String>,
    /// Workflow lineage depth inherited from the parent.
    pub workflow_nesting_depth: usize,
    /// Parent leader session id.
    pub parent_session_id: String,
    /// Whether `agent_type` was explicitly provided by the caller.
    pub agent_type_explicit: bool,
    /// Whether the caller should wait for this request's handoff.
    pub wait_for_completion: bool,
    /// Parent permission broker used for sensitive permission prompts.
    pub permission_broker: Option<Arc<dyn PermissionBroker>>,
    /// Whether this teammate has no interactive permission consumer.
    pub permission_prompts_unavailable: bool,
}

impl std::fmt::Debug for TeammateSpawnSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeammateSpawnSpec")
            .field("team_name", &self.team_name)
            .field("name", &self.name)
            .field("prompt", &self.prompt)
            .field("agent_type", &self.agent_type)
            .field("model", &self.model)
            .field("model_profile", &self.model_profile)
            .field("provider", &self.provider)
            .field("mode", &self.mode)
            .field("effort", &self.effort)
            .field("description", &self.description)
            .field("has_system", &self.system.is_some())
            .field("cwd", &self.cwd)
            .field(
                "additional_working_directories",
                &self.additional_working_directories,
            )
            .field("workflow_nesting_depth", &self.workflow_nesting_depth)
            .field("parent_session_id", &self.parent_session_id)
            .field("agent_type_explicit", &self.agent_type_explicit)
            .field("wait_for_completion", &self.wait_for_completion)
            .field("has_permission_broker", &self.permission_broker.is_some())
            .field(
                "permission_prompts_unavailable",
                &self.permission_prompts_unavailable,
            )
            .finish()
    }
}

/// Compact result returned to the main agent after one teammate turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateHandoff {
    pub status: String,
    pub summary: String,
    pub error: Option<String>,
}

/// Lightweight teammate state exposed to the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateRosterEntry {
    pub name: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub current_task: Option<String>,
    pub last_result: Option<String>,
}

/// Result of a successful teammate spawn or dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateSpawnResult {
    /// Team name the teammate joined.
    pub team_name: String,
    /// Unique teammate agent id.
    pub agent_id: String,
    /// Registry task id backing the teammate runtime.
    pub task_id: String,
    /// True when this request was sent to an existing teammate.
    pub reused: bool,
    /// Compact foreground result. Background dispatches return `None`.
    pub handoff: Option<TeammateHandoff>,
}

/// Runtime hook that owns teammate spawning / lifecycle.
#[async_trait]
pub trait TeamManager: Send + Sync {
    /// Spawn a teammate.
    async fn spawn_teammate(&self, spec: TeammateSpawnSpec) -> Result<TeammateSpawnResult, String>;

    /// Return the team currently owned by one main session, if known.
    fn team_name_for_session(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// Return a lightweight roster for one main session.
    fn teammate_roster(&self, _session_id: &str) -> Vec<TeammateRosterEntry> {
        Vec::new()
    }

    /// Send a plain-text message to a teammate.
    async fn send_message(
        &self,
        team_name: &str,
        recipient: &str,
        message: String,
    ) -> Result<(), ToolErrorPresentation>;

    /// Send a plain-text message to a running task by task id.
    ///
    /// Coordinator-mode `Agent` workers are registered as local-agent
    /// tasks, not in-process teammates, so they may not have an active
    /// team context. Implementations that own a task registry can
    /// override this to queue a mid-flight message for those workers.
    async fn send_message_to_task(
        &self,
        task_id: &str,
        _message: String,
    ) -> Result<(), ToolErrorPresentation> {
        Err(ToolErrorPresentation::new(
            "agent_not_routable",
            format!("Agent \"{task_id}\" is not available."),
            format!(
                "SendMessage target `{task_id}` is not a teammate and this runtime cannot route by task id"
            ),
        ))
    }

    /// Request graceful shutdown of a teammate.
    async fn request_shutdown(
        &self,
        team_name: &str,
        recipient: &str,
        reason: Option<String>,
    ) -> Result<String, String>;

    /// Submit a plan for leader approval.
    async fn request_plan_approval(
        &self,
        team_name: &str,
        agent_name: &str,
        plan_content: String,
    ) -> Result<String, String>;

    /// Close and remove all teammates owned by one main session.
    fn close_session(&self, _session_id: &str) {}

    /// Delete a team after all teammates have exited.
    async fn delete_team(&self, team_name: &str) -> Result<(), String>;
}

/// Stable typed name of the seat a front end resolves a session's team
/// manager off.
pub const TEAM_MANAGER_SERVICE: &str = "team-manager";

/// Typed definition for the kernel's `team-manager` seat.
pub struct TeamManagerService;

impl rebon_kernel::Service for TeamManagerService {
    type Interface = dyn TeamManagerSource;
    const NAME: &'static str = TEAM_MANAGER_SERVICE;
}

/// The provider behind the seat: whatever can build a team manager for a
/// session.
///
/// One provider, filled by the `Agent` plugin. With it switched off
/// nothing is on the seat and no manager reaches `ToolContext`, which is the
/// case `TeamCreate` / `SendMessage` / `TeamDelete` already answer with "no
/// team runtime here" rather than a panic.
pub trait TeamManagerSource: Send + Sync {
    /// Build the manager this session's teammates live in.
    ///
    /// Called once per session, on the path that builds the executor. `Err` is
    /// a wiring fault (a handle of the wrong shape), not "no teams here" —
    /// that case is the seat being empty.
    fn for_session(&self, request: TeamManagerRequest) -> Result<Arc<dyn TeamManager>, String>;
}

/// What only the front end knows about a session's teammates.
///
/// The same shape as [`crate::agent::SubAgentSpawnerRequest`], and for the
/// same reason: a teammate turn is a sub-agent turn that outlives the call
/// that started it, so it is configured with the same engine, client, models
/// and base filter.
pub struct TeamManagerRequest {
    /// The session's engine, as an `Arc` — a teammate outlives the turn that
    /// spawned it, so unlike the spawner this half holds it strongly.
    pub engine: crate::agent::SubAgentRuntimeHandle,
    /// The session's task runtime: a teammate is a row in the task registry.
    pub task_runtime: crate::agent::SubAgentRuntimeHandle,
    /// The client a teammate's turns go to.
    pub client: Arc<dyn rebon_api::ModelClient>,
    /// The model a teammate that names none inherits — the parent's.
    pub default_model: String,
    /// `agents.json` model selections, by agent type / category / alias.
    pub model_config: rebon_types::SubAgentModelConfig,
    /// The active provider's profile map, for `modelProfile` names.
    pub model_profiles: rebon_types::ModelProfileMap,
    /// Which provider, model and effort each teammate turn resolves to.
    pub model_router: Arc<dyn rebon_agent_core::model_router::AgentModelRouter>,
    /// The base tool filter every teammate turn snapshots and intersects with
    /// the teammate deny-list.
    pub base_filter: Option<crate::SharedToolFilter>,
}
