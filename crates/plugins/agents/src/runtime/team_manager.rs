use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rebon_api::{ModelClient, Usage};
use rebon_core::Engine;
use rebon_tool::tasks::{
    list_tasks, task_list_notification, update_task, TaskListStatus, TaskPatch,
};
use rebon_tool::team_files::now_wall_ms;
use rebon_tool::{
    append_team_member, bind_session_team, clear_current_team_name, clear_session_team_bindings,
    drain_unread_mailbox, format_agent_id, is_session_default_team_name, mailbox_notification,
    read_team_file, remove_member_by_agent_id, remove_session_default_team, sanitize_team_name,
    set_team_member_active, set_team_member_mode, write_mailbox_message,
    AutoApproveExceptSensitivePermissionBroker, SharedToolFilter, TeamIdentityContext,
    TeamMailboxMessage, TeamManager, TeamMember, TeammateHandoff, TeammateRosterEntry,
    TeammateSpawnResult, TeammateSpawnSpec, ToolContext, ToolFilter,
};
use rebon_tools_core::ToolErrorPresentation;
use rebon_types::{ModelProfileMap, PromptCancel, SubAgentModelConfig};
use tokio::sync::oneshot;

use crate::runtime::worker::{
    spawn_worker, WorkerEvent, WorkerHandle, WorkerResult, WorkerSpec, WorkerStatus,
};
use rebon_agent_core::model_router::{
    parse_reasoning_effort, AgentModelRouter, ModelRouteRequest, SingleProviderModelRouter,
};
use rebon_plugin_tasks::runtime::{
    assistant_text_delta, describe_worker_after_tool_activity, describe_worker_tool_activity,
    enqueue_teammate_request, finish_in_process_teammate, generate_task_id,
    inject_user_message_to_teammate, mark_in_process_teammate_idle,
    mark_in_process_teammate_running, push_bounded_agent_transcript,
    push_in_process_teammate_turn_prompt, register_in_process_teammate_task_with_cancel,
    request_teammate_shutdown, revive_in_process_teammate_task, send_message_to_local_agent_task,
    set_in_process_teammate_awaiting_plan_approval, set_in_process_teammate_current_task,
    set_in_process_teammate_permission_mode, take_pending_teammate_request,
    upsert_bounded_agent_thinking, InProcessTeammateTaskSpec, LocalAgentTranscriptEntry, TaskData,
    TaskId, TaskKind, TaskLiveEventKind, TaskRegistry, TaskSnapshot, TaskStatus, TaskTurnToken,
    TeammateIdentity, TeammateRequest,
};

const TEAM_LEAD_NAME: &str = "team-lead";
const DEFAULT_PERMISSION_MODE: &str = "default";
const PLAN_PERMISSION_MODE: &str = "plan";
const TEAMMATE_DENIED_TOOLS: [&str; 5] = [
    "Agent",
    "TeamCreate",
    "TeamDelete",
    "SendMessage",
    "TaskStop",
];

/// Drive one in-process teammate worker turn while the tasks plugin owns the
/// registry and lifecycle state. The coordinator keeps this event bridge
/// because [`WorkerHandle`] is coordinator-owned.
async fn drive_in_process_teammate_worker_turn(
    registry: &TaskRegistry,
    turn: &TaskTurnToken,
    mut worker: WorkerHandle,
) -> WorkerResult {
    let mut assistant_snapshot = String::new();
    let mut thinking_snapshot = String::new();
    while let Some(event) = worker.next_event().await {
        match event {
            WorkerEvent::AssistantText { text } => {
                let delta = assistant_text_delta(&assistant_snapshot, &text);
                assistant_snapshot = text.clone();
                registry.update_task_turn(turn, |snap| {
                    if snap.status.is_terminal() {
                        return;
                    }
                    snap.last_progress = Some(text.clone());
                    if let TaskData::InProcessTeammate(data) = &mut snap.data {
                        data.streaming_text = Some(text.clone());
                        if data.transcript.last().is_some_and(|entry| {
                            matches!(entry, LocalAgentTranscriptEntry::User { .. })
                        }) {
                            push_bounded_agent_transcript(
                                &mut data.transcript,
                                LocalAgentTranscriptEntry::Assistant { text: text.clone() },
                            );
                        }
                    }
                });
                if !delta.is_empty() {
                    registry.record_task_turn_event(
                        turn,
                        TaskLiveEventKind::AssistantTextDelta {
                            delta,
                            snapshot: text,
                        },
                    );
                }
            }
            WorkerEvent::Thinking { text } => {
                let delta = assistant_text_delta(&thinking_snapshot, &text);
                thinking_snapshot = text.clone();
                registry.update_task_turn(turn, |snap| {
                    if snap.status.is_terminal() {
                        return;
                    }
                    snap.last_progress = Some(format!("Thinking: {text}"));
                    if let TaskData::InProcessTeammate(data) = &mut snap.data {
                        upsert_bounded_agent_thinking(&mut data.transcript, text.clone());
                    }
                });
                if !delta.is_empty() {
                    registry.record_task_turn_event(
                        turn,
                        TaskLiveEventKind::ThinkingDelta {
                            delta,
                            snapshot: text,
                        },
                    );
                }
            }
            WorkerEvent::IterationComplete { text, .. } => {
                assistant_snapshot.clear();
                thinking_snapshot.clear();
                registry.update_task_turn(turn, |snap| {
                    if snap.status.is_terminal() {
                        return;
                    }
                    snap.last_progress = Some(text.clone());
                    if let TaskData::InProcessTeammate(data) = &mut snap.data {
                        data.streaming_text = None;
                        if !text.trim().is_empty() {
                            match data.transcript.last_mut() {
                                Some(LocalAgentTranscriptEntry::Assistant { text: existing }) => {
                                    *existing = text.clone();
                                }
                                _ => push_bounded_agent_transcript(
                                    &mut data.transcript,
                                    LocalAgentTranscriptEntry::Assistant { text: text.clone() },
                                ),
                            }
                        }
                    }
                });
                registry.record_task_turn_event(
                    turn,
                    TaskLiveEventKind::AssistantTurnComplete { text },
                );
            }
            WorkerEvent::ToolStart {
                name,
                input,
                tool_use_id,
            } => {
                let activity = describe_worker_tool_activity(&name, &input);
                registry.update_task_turn(turn, |snap| {
                    if snap.status.is_terminal() {
                        return;
                    }
                    snap.last_progress = Some(activity.clone());
                    if let TaskData::InProcessTeammate(data) = &mut snap.data {
                        data.tool_use_count = data.tool_use_count.saturating_add(1);
                        push_bounded_agent_transcript(
                            &mut data.transcript,
                            LocalAgentTranscriptEntry::ToolStart {
                                tool_use_id: tool_use_id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                                activity: activity.clone(),
                            },
                        );
                    }
                });
                registry.record_task_turn_event(
                    turn,
                    TaskLiveEventKind::ToolStart {
                        tool_use_id,
                        name,
                        input,
                    },
                );
            }
            WorkerEvent::ToolProgress {
                name,
                message,
                tool_use_id,
            } => {
                if let Some(msg) = message {
                    registry.update_task_turn(turn, |snap| {
                        if snap.status.is_terminal() {
                            return;
                        }
                        snap.last_progress = Some(format!("{name}: {msg}"));
                        if let TaskData::InProcessTeammate(data) = &mut snap.data {
                            push_bounded_agent_transcript(
                                &mut data.transcript,
                                LocalAgentTranscriptEntry::ToolProgress {
                                    tool_use_id: tool_use_id.clone(),
                                    name: name.clone(),
                                    message: msg.clone(),
                                },
                            );
                        }
                    });
                    registry.record_task_turn_event(
                        turn,
                        TaskLiveEventKind::ToolProgress {
                            tool_use_id,
                            name,
                            message: msg,
                        },
                    );
                }
            }
            WorkerEvent::PermissionQuery(query) => {
                let _ = query
                    .response_tx
                    .send(rebon_core::permission::PermissionAnswer::Cancelled);
            }
            WorkerEvent::ToolFinish {
                name,
                outcome,
                tool_use_id,
            } => {
                let summary = match &outcome {
                    Ok(_) => format!("{name} ok"),
                    Err(err) => format!("{name} error: {err}"),
                };
                let next_activity = describe_worker_after_tool_activity(&name);
                registry.update_task_turn(turn, |snap| {
                    if snap.status.is_terminal() {
                        return;
                    }
                    snap.last_progress = Some(next_activity.clone());
                    if let TaskData::InProcessTeammate(data) = &mut snap.data {
                        push_bounded_agent_transcript(
                            &mut data.transcript,
                            LocalAgentTranscriptEntry::ToolFinish {
                                tool_use_id: tool_use_id.clone(),
                                name: name.clone(),
                                ok: outcome.is_ok(),
                                summary: summary.clone(),
                                outcome: outcome.clone(),
                            },
                        );
                    }
                });
                registry.record_task_turn_event(
                    turn,
                    TaskLiveEventKind::ToolFinish {
                        tool_use_id,
                        name,
                        outcome,
                    },
                );
            }
            WorkerEvent::Completed(final_result) => {
                registry.update_task_turn(turn, |snap| {
                    if snap.status.is_terminal() {
                        return;
                    }
                    if let TaskData::InProcessTeammate(data) = &mut snap.data {
                        data.streaming_text = None;
                        data.token_count = data.token_count.saturating_add(
                            u64::from(final_result.total_usage.billed_input_tokens())
                                + u64::from(final_result.total_usage.output_tokens),
                        );
                        if !final_result.final_text.trim().is_empty() {
                            match data.transcript.last_mut() {
                                Some(LocalAgentTranscriptEntry::Assistant { text })
                                    if text == &final_result.final_text => {}
                                Some(LocalAgentTranscriptEntry::Assistant { text }) => {
                                    *text = final_result.final_text.clone();
                                }
                                _ => push_bounded_agent_transcript(
                                    &mut data.transcript,
                                    LocalAgentTranscriptEntry::Assistant {
                                        text: final_result.final_text.clone(),
                                    },
                                ),
                            }
                        }
                    }
                });
                return final_result;
            }
        }
    }
    WorkerResult {
        status: WorkerStatus::Failed,
        final_text: String::new(),
        stop_reason: None,
        total_usage: Usage::default(),
        cumulative_output_tokens: 0,
        tool_calls: Vec::new(),
        context_reset_occurred: false,
        error: Some("worker event stream closed unexpectedly".into()),
    }
}

fn normalize_teammate_permission_mode(mode: Option<&str>) -> String {
    mode.unwrap_or(DEFAULT_PERMISSION_MODE).to_string()
}

fn teammate_plan_mode_required(permission_mode: &str) -> bool {
    permission_mode == PLAN_PERMISSION_MODE
}

/// Tools a plan-mode teammate must not reach: every tool that writes a file
/// the caller named, asked of the tools themselves rather than listed here.
/// Mirrors the read-only deny list used by the built-in Explore/Plan agents
/// (`rebon_tool::builtin_agents::READ_ONLY_DENIED_TOOLS`) minus
/// `Agent`/`ExitPlanMode`, which the teammate filter already handles:
/// plan mode is enforced at the tool layer, not just via the prompt
/// preamble.
fn plan_mode_denied_tools() -> Vec<&'static str> {
    rebon_tools_core::tool_names_of_kind(rebon_tools_core::ToolKind::FileEdit)
}

fn teammate_worker_filter(plan_mode: bool, base_filter: Option<&SharedToolFilter>) -> ToolFilter {
    let mut filter = ToolFilter::unrestricted().with_deny(TEAMMATE_DENIED_TOOLS);
    if plan_mode {
        filter = filter.with_deny(plan_mode_denied_tools());
    }
    match base_filter {
        // Same rule as the non-coordinator sub-agent path: intersect
        // with the session base filter so a teammate never sees more
        // tools than an equivalent sub-agent would.
        Some(base) => base.current().intersect(&filter),
        None => filter,
    }
}

#[derive(Debug, serde::Deserialize)]
struct PlanApprovalResponse {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "requestId")]
    request_id: String,
    approved: bool,
    feedback: Option<String>,
    #[serde(rename = "permissionMode")]
    permission_mode: Option<String>,
}

struct TeamManagerLifecycle {
    registry: TaskRegistry,
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<TeammateHandoff>>>>,
    recipes: Arc<Mutex<HashMap<TaskId, TeammateSpawnSpec>>>,
    sessions: Mutex<HashSet<String>>,
    closed_sessions: Mutex<HashSet<String>>,
}

impl TeamManagerLifecycle {
    fn try_track_session(&self, session_id: &str) -> bool {
        if session_id.trim().is_empty() {
            return true;
        }
        if self
            .closed_sessions
            .lock()
            .expect("closed teammate session registry poisoned")
            .contains(session_id)
        {
            return false;
        }
        self.sessions
            .lock()
            .expect("teammate session registry poisoned")
            .insert(session_id.to_string());
        true
    }

    fn close_session(&self, session_id: &str) {
        if session_id.trim().is_empty() {
            return;
        }
        self.closed_sessions
            .lock()
            .expect("closed teammate session registry poisoned")
            .insert(session_id.to_string());
        let mut task_ids = self
            .recipes
            .lock()
            .expect("teammate recipe registry poisoned")
            .iter()
            .filter_map(|(task_id, spec)| {
                (spec.parent_session_id == session_id).then(|| task_id.clone())
            })
            .collect::<HashSet<_>>();
        task_ids.extend(
            self.registry
                .snapshots()
                .into_iter()
                .filter_map(|snapshot| match &snapshot.data {
                    TaskData::InProcessTeammate(data)
                        if data.identity.parent_session_id == session_id =>
                    {
                        Some(snapshot.id)
                    }
                    _ => None,
                }),
        );
        self.recipes
            .lock()
            .expect("teammate recipe registry poisoned")
            .retain(|task_id, spec| {
                spec.parent_session_id != session_id && !task_ids.contains(task_id)
            });
        self.waiters
            .lock()
            .expect("teammate waiter registry poisoned")
            .retain(|request_id, _| {
                !task_ids
                    .iter()
                    .any(|task_id| request_id.starts_with(&format!("{task_id}:")))
            });
        self.registry.close_owner_session(session_id);
        self.sessions
            .lock()
            .expect("teammate session registry poisoned")
            .remove(session_id);
        if let Err(error) = clear_session_team_bindings(session_id) {
            tracing::warn!(
                session_id,
                %error,
                "failed to clear persisted team binding during session teardown"
            );
        }
        if let Err(error) = remove_session_default_team(session_id) {
            tracing::warn!(
                session_id,
                %error,
                "failed to remove session default team during session teardown"
            );
        }
    }

    fn close_all(&self) {
        let mut session_ids = self
            .sessions
            .lock()
            .expect("teammate session registry poisoned")
            .clone();
        let recipe_sessions = self
            .recipes
            .lock()
            .expect("teammate recipe registry poisoned")
            .values()
            .map(|spec| spec.parent_session_id.clone())
            .collect::<Vec<_>>();
        session_ids.extend(recipe_sessions);
        session_ids.extend(
            self.registry
                .snapshots()
                .into_iter()
                .filter_map(|snapshot| match &snapshot.data {
                    TaskData::InProcessTeammate(data) => {
                        Some(data.identity.parent_session_id.clone())
                    }
                    _ => None,
                }),
        );
        for session_id in session_ids {
            if !session_id.trim().is_empty() {
                self.close_session(&session_id);
            }
        }
    }
}

impl Drop for TeamManagerLifecycle {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// In-process team manager for the local Rust runtime.
#[derive(Clone)]
pub struct InProcessTeamManager {
    engine: Arc<Engine>,
    client: Arc<dyn ModelClient>,
    registry: TaskRegistry,
    default_model: String,
    model_config: SubAgentModelConfig,
    model_profiles: ModelProfileMap,
    model_router: Option<Arc<dyn AgentModelRouter>>,
    base_filter: Option<SharedToolFilter>,
    dispatch_lock: Arc<Mutex<()>>,
    request_sequence: Arc<AtomicU64>,
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<TeammateHandoff>>>>,
    recipes: Arc<Mutex<HashMap<TaskId, TeammateSpawnSpec>>>,
    lifecycle: Arc<TeamManagerLifecycle>,
}

impl InProcessTeamManager {
    /// Build a new in-process team manager.
    pub fn new(
        engine: Arc<Engine>,
        client: Arc<dyn ModelClient>,
        registry: TaskRegistry,
        default_model: impl Into<String>,
    ) -> Self {
        let waiters = Arc::new(Mutex::new(HashMap::new()));
        let recipes = Arc::new(Mutex::new(HashMap::new()));
        let lifecycle = Arc::new(TeamManagerLifecycle {
            registry: registry.clone(),
            waiters: waiters.clone(),
            recipes: recipes.clone(),
            sessions: Mutex::new(HashSet::new()),
            closed_sessions: Mutex::new(HashSet::new()),
        });
        Self {
            engine,
            client,
            registry,
            default_model: default_model.into(),
            model_config: SubAgentModelConfig::default(),
            model_profiles: ModelProfileMap::default(),
            model_router: None,
            base_filter: None,
            dispatch_lock: Arc::new(Mutex::new(())),
            request_sequence: Arc::new(AtomicU64::new(1)),
            waiters,
            recipes,
            lifecycle,
        }
    }

    /// Attach the session's shared base [`ToolFilter`] handle. Every
    /// teammate turn snapshots it and intersects it with the teammate
    /// deny-list, matching the sub-agent spawner's behavior.
    pub fn with_shared_base_filter(mut self, filter: SharedToolFilter) -> Self {
        self.base_filter = Some(filter);
        self
    }

    pub fn with_model_config(mut self, config: SubAgentModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub fn with_model_profiles(mut self, profiles: ModelProfileMap) -> Self {
        self.model_profiles = profiles;
        self
    }

    pub fn with_model_router(mut self, router: Arc<dyn AgentModelRouter>) -> Self {
        self.model_router = Some(router);
        self
    }

    fn next_request_id(&self, task_id: &TaskId) -> String {
        format!(
            "{}:{}",
            task_id,
            self.request_sequence.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn register_waiter(
        &self,
        request_id: &str,
        wait: bool,
    ) -> Option<oneshot::Receiver<TeammateHandoff>> {
        if !wait {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        self.waiters
            .lock()
            .expect("teammate waiter registry poisoned")
            .insert(request_id.to_string(), sender);
        Some(receiver)
    }

    async fn await_handoff(
        receiver: Option<oneshot::Receiver<TeammateHandoff>>,
    ) -> Result<Option<TeammateHandoff>, String> {
        match receiver {
            Some(receiver) => receiver
                .await
                .map(Some)
                .map_err(|_| "teammate stopped before returning a handoff".to_string()),
            None => Ok(None),
        }
    }

    fn spawn_runtime(
        &self,
        cancel: PromptCancel,
        task_id: TaskId,
        identity: TeammateIdentity,
        spec: TeammateSpawnSpec,
        initial_request: Option<TeammateRequest>,
    ) {
        let engine = self.engine.clone();
        let client = self.client.clone();
        let registry = self.registry.clone();
        let default_model = self.default_model.clone();
        let model_config = self.model_config.clone();
        let model_profiles = self.model_profiles.clone();
        let model_router = self.model_router();
        let base_filter = self.base_filter.clone();
        let waiters = self.waiters.clone();
        tokio::spawn(async move {
            run_teammate_loop(
                engine,
                client,
                registry,
                cancel,
                task_id,
                identity,
                spec,
                initial_request,
                default_model,
                model_config,
                model_profiles,
                model_router,
                base_filter,
                waiters,
            )
            .await;
        });
    }

    fn remove_waiter(&self, request_id: &str) {
        self.waiters
            .lock()
            .expect("teammate waiter registry poisoned")
            .remove(request_id);
    }

    fn find_teammate_snapshot(&self, parent_session_id: &str, name: &str) -> Option<TaskSnapshot> {
        self.registry
            .snapshots()
            .into_iter()
            .filter(|snapshot| matches!(snapshot.data, TaskData::InProcessTeammate(_)))
            .filter(|snapshot| {
                snapshot
                    .metadata_str("agent_name")
                    .is_some_and(|agent_name| agent_name.eq_ignore_ascii_case(name))
            })
            .find(|snapshot| {
                snapshot
                    .metadata_str("parent_session_id")
                    .unwrap_or_default()
                    == parent_session_id
            })
    }

    fn existing_agent_type(snapshot: &TaskSnapshot) -> Option<String> {
        snapshot
            .metadata_str("agent_type")
            .map(str::trim)
            .filter(|agent_type| !agent_type.is_empty())
            .map(str::to_string)
    }

    fn model_router(&self) -> Arc<dyn AgentModelRouter> {
        self.model_router.clone().unwrap_or_else(|| {
            Arc::new(
                SingleProviderModelRouter::new(self.client.clone(), self.default_model.clone())
                    .with_provider_name(self.client.provider_name().to_string())
                    .with_model_config(self.model_config.clone())
                    .with_model_profiles(self.model_profiles.clone()),
            )
        })
    }
}

#[async_trait]
impl TeamManager for InProcessTeamManager {
    async fn spawn_teammate(
        &self,
        mut spec: TeammateSpawnSpec,
    ) -> Result<TeammateSpawnResult, String> {
        let (mut result, waiter) = {
            let _dispatch = self
                .dispatch_lock
                .lock()
                .expect("teammate dispatch lock poisoned");
            if !self.lifecycle.try_track_session(&spec.parent_session_id) {
                return Err(format!(
                    "session `{}` is closed and cannot accept teammate work",
                    spec.parent_session_id
                ));
            }
            bind_session_team(&spec.team_name, &spec.parent_session_id)
                .map_err(|error| error.to_string())?;
            if let Some(existing) = self.find_teammate_snapshot(&spec.parent_session_id, &spec.name)
            {
                let task_id = existing.id.clone();
                let existing_type = Self::existing_agent_type(&existing);
                if spec.agent_type_explicit {
                    if let (Some(requested), Some(existing_type)) =
                        (spec.agent_type.as_deref(), existing_type.as_deref())
                    {
                        if !requested.eq_ignore_ascii_case(existing_type) {
                            return Err(format!(
                                "teammate `{}` already has agent type `{existing_type}`; cannot redispatch it as `{requested}`",
                                spec.name
                            ));
                        }
                    }
                } else {
                    spec.agent_type = existing_type;
                }

                let identity = match &existing.data {
                    TaskData::InProcessTeammate(data) => data.identity.clone(),
                    _ => unreachable!("teammate lookup returned a non-teammate task"),
                };
                if identity.team_name != spec.team_name {
                    return Err(format!(
                        "teammate `{}` already belongs to team `{}` for this session",
                        spec.name, identity.team_name
                    ));
                }

                let request_id = self.next_request_id(&task_id);
                let request = TeammateRequest {
                    request_id: request_id.clone(),
                    message: spec.prompt.clone(),
                };
                let waiter = self.register_waiter(&request_id, spec.wait_for_completion);

                if existing.status.is_terminal() {
                    let cancel = PromptCancel::new();
                    let Some(identity) = revive_in_process_teammate_task(
                        &self.registry,
                        &task_id,
                        cancel.clone(),
                        request,
                    ) else {
                        self.remove_waiter(&request_id);
                        return Err(format!("failed to revive teammate `{}`", spec.name));
                    };
                    let mut recipe = self
                        .recipes
                        .lock()
                        .expect("teammate recipe registry poisoned")
                        .get(&task_id)
                        .cloned()
                        .unwrap_or_else(|| spec.clone());
                    recipe.prompt = spec.prompt.clone();
                    recipe.agent_type = spec.agent_type.clone();
                    recipe.wait_for_completion = false;
                    self.recipes
                        .lock()
                        .expect("teammate recipe registry poisoned")
                        .insert(task_id.clone(), recipe.clone());
                    self.spawn_runtime(cancel, task_id.clone(), identity.clone(), recipe, None);
                } else if !enqueue_teammate_request(&self.registry, &task_id, request) {
                    self.remove_waiter(&request_id);
                    return Err(format!(
                        "teammate `{}` cannot accept another request yet",
                        spec.name
                    ));
                }

                (
                    TeammateSpawnResult {
                        team_name: identity.team_name,
                        agent_id: identity.agent_id,
                        task_id: task_id.to_string(),
                        reused: true,
                        handoff: None,
                    },
                    waiter,
                )
            } else {
                let agent_id = format_agent_id(&spec.name, &spec.team_name);
                let task_id = generate_task_id(TaskKind::InProcessTeammate);
                let permission_mode = normalize_teammate_permission_mode(spec.mode.as_deref());
                let plan_mode_required = teammate_plan_mode_required(&permission_mode);
                let identity = TeammateIdentity {
                    agent_id: agent_id.clone(),
                    agent_name: spec.name.clone(),
                    team_name: spec.team_name.clone(),
                    color: None,
                    plan_mode_required,
                    parent_session_id: spec.parent_session_id.clone(),
                };
                let task_spec = InProcessTeammateTaskSpec {
                    id: task_id.clone(),
                    identity: identity.clone(),
                    prompt: spec.prompt.clone(),
                    model: spec.model.clone(),
                    model_profile: spec.model_profile.clone(),
                    permission_mode: permission_mode.clone(),
                    agent_type: spec.agent_type.clone(),
                    description: spec.description.clone(),
                };
                let request_id = self.next_request_id(&task_id);
                let request = TeammateRequest {
                    request_id: request_id.clone(),
                    message: spec.prompt.clone(),
                };
                let waiter = self.register_waiter(&request_id, spec.wait_for_completion);

                let cwd = spec.cwd.clone().unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| ".".into())
                });
                if let Err(error) = append_team_member(
                    &spec.team_name,
                    TeamMember {
                        agent_id: agent_id.clone(),
                        name: spec.name.clone(),
                        agent_type: spec.agent_type.clone(),
                        model: spec.model.clone(),
                        model_profile: spec.model_profile.clone(),
                        prompt: Some(spec.prompt.clone()),
                        color: None,
                        plan_mode_required: Some(plan_mode_required),
                        joined_at: now_wall_ms(),
                        tmux_pane_id: task_id.as_str().to_string(),
                        cwd,
                        worktree_path: None,
                        backend_type: Some("in-process".into()),
                        is_active: Some(true),
                        mode: Some(permission_mode),
                        subscriptions: Vec::new(),
                    },
                ) {
                    self.remove_waiter(&request_id);
                    return Err(error.to_string());
                }

                let cancel = PromptCancel::new();
                register_in_process_teammate_task_with_cancel(
                    &self.registry,
                    task_spec,
                    cancel.clone(),
                );
                self.recipes
                    .lock()
                    .expect("teammate recipe registry poisoned")
                    .insert(task_id.clone(), spec.clone());
                self.spawn_runtime(
                    cancel,
                    task_id.clone(),
                    identity,
                    spec.clone(),
                    Some(request),
                );

                (
                    TeammateSpawnResult {
                        team_name: spec.team_name.clone(),
                        agent_id,
                        task_id: task_id.to_string(),
                        reused: false,
                        handoff: None,
                    },
                    waiter,
                )
            }
        };

        result.handoff = Self::await_handoff(waiter).await?;
        Ok(result)
    }

    async fn send_message(
        &self,
        team_name: &str,
        recipient: &str,
        message: String,
    ) -> Result<(), ToolErrorPresentation> {
        let _dispatch = self
            .dispatch_lock
            .lock()
            .expect("teammate dispatch lock poisoned");
        if let Some(snapshot) = self.find_team_teammate_snapshot(team_name, recipient) {
            let task_id = snapshot.id.clone();
            if snapshot.status.is_terminal() {
                let Some(mut recipe) = self
                    .recipes
                    .lock()
                    .expect("teammate recipe registry poisoned")
                    .get(&task_id)
                    .cloned()
                else {
                    return Err(ToolErrorPresentation::new(
                        "agent_restart_failed",
                        format!("Agent \"{recipient}\" could not be restarted."),
                        "The teammate runtime recipe is no longer available in this session. Redispatch it with the Agent tool.".to_string(),
                    ));
                };
                let request = TeammateRequest {
                    request_id: self.next_request_id(&task_id),
                    message,
                };
                let cancel = PromptCancel::new();
                let Some(identity) = revive_in_process_teammate_task(
                    &self.registry,
                    &task_id,
                    cancel.clone(),
                    request,
                ) else {
                    return Err(ToolErrorPresentation::new(
                        "agent_restart_failed",
                        format!("Agent \"{recipient}\" could not be restarted."),
                        format!(
                            "Agent \"{recipient}\" changed state while SendMessage was restarting it. Retry the message."
                        ),
                    ));
                };
                recipe.wait_for_completion = false;
                self.spawn_runtime(cancel, task_id, identity, recipe, None);
                return Ok(());
            }
            if inject_user_message_to_teammate(&self.registry, &task_id, message) {
                return Ok(());
            }
            return Err(ToolErrorPresentation::new(
                "agent_queue_full",
                format!("Agent \"{recipient}\" cannot accept another message yet."),
                format!(
                    "Agent \"{recipient}\" in team \"{team_name}\" has reached its pending-message limit. Wait for it to process queued work before sending another message."
                ),
            ));
        }

        match send_message_to_local_agent_task(&self.registry, recipient, message) {
            Ok(()) => Ok(()),
            Err(error) if error.code != "agent_not_found" => Err(error),
            Err(_) => Err(ToolErrorPresentation::new(
                "agent_not_found",
                format!("Agent \"{recipient}\" was not found."),
                format!(
                    "Agent \"{recipient}\" has no teammate task in team \"{team_name}\". Verify the teammate name, or use Agent with that name to create it."
                ),
            )),
        }
    }

    async fn send_message_to_task(
        &self,
        task_id: &str,
        message: String,
    ) -> Result<(), ToolErrorPresentation> {
        let task_id = TaskId::new(task_id.to_string());
        if let Some(snapshot) = self.registry.snapshot(&task_id) {
            if let TaskData::InProcessTeammate(data) = &snapshot.data {
                return self
                    .send_message(&data.identity.team_name, &data.identity.agent_name, message)
                    .await;
            }
        }
        send_message_to_local_agent_task(&self.registry, task_id.as_str(), message)
    }

    async fn request_shutdown(
        &self,
        team_name: &str,
        recipient: &str,
        _reason: Option<String>,
    ) -> Result<String, String> {
        let Some(task_id) = self.find_teammate_task_id(team_name, recipient) else {
            return Err(format!(
                "teammate `{recipient}` not found in team `{team_name}`"
            ));
        };
        if !request_teammate_shutdown(&self.registry, &task_id) {
            return Err(format!(
                "shutdown already requested or teammate `{recipient}` is no longer running"
            ));
        }
        Ok(format!("shutdown-{}-{}", recipient, now_wall_ms()))
    }

    async fn request_plan_approval(
        &self,
        team_name: &str,
        agent_name: &str,
        plan_content: String,
    ) -> Result<String, String> {
        let Some(task_id) = self.find_teammate_task_id(team_name, agent_name) else {
            return Err(format!(
                "teammate `{agent_name}` not found in team `{team_name}`"
            ));
        };
        let request_id = format!("plan-approval-{}-{}", agent_name, now_wall_ms());
        let payload = serde_json::json!({
            "type": "plan_approval_request",
            "from": agent_name,
            "timestamp": now_wall_ms().to_string(),
            "planFilePath": "<inline-plan>",
            "planContent": plan_content,
            "requestId": request_id,
        });
        write_mailbox_message(
            team_name,
            TEAM_LEAD_NAME,
            TeamMailboxMessage {
                from: agent_name.to_string(),
                text: payload.to_string(),
                timestamp: now_wall_ms().to_string(),
                read: false,
                color: None,
                summary: Some("plan approval request".into()),
            },
        )
        .map_err(|err| err.to_string())?;
        let _ = set_in_process_teammate_awaiting_plan_approval(&self.registry, &task_id, true);
        Ok(request_id)
    }

    fn team_name_for_session(&self, parent_session_id: &str) -> Option<String> {
        let mut team_names = self
            .registry
            .snapshots()
            .into_iter()
            .filter_map(|snapshot| {
                let TaskData::InProcessTeammate(data) = &snapshot.data else {
                    return None;
                };
                (data.identity.parent_session_id == parent_session_id)
                    .then(|| data.identity.team_name.clone())
            })
            .collect::<Vec<_>>();
        team_names.sort();
        team_names.dedup();
        team_names.into_iter().find(|team_name| {
            read_team_file(team_name)
                .ok()
                .flatten()
                .is_some_and(|team| {
                    team.lead_session_id
                        .as_deref()
                        .is_none_or(|session_id| session_id == parent_session_id)
                })
        })
    }

    fn teammate_roster(&self, parent_session_id: &str) -> Vec<TeammateRosterEntry> {
        let mut entries = self
            .registry
            .snapshots()
            .into_iter()
            .filter_map(|snapshot| {
                let TaskData::InProcessTeammate(data) = &snapshot.data else {
                    return None;
                };
                if data.identity.parent_session_id != parent_session_id {
                    return None;
                }
                let status = if snapshot.status == TaskStatus::Failed {
                    "failed-restartable"
                } else if snapshot.status.is_terminal() {
                    return None;
                } else if data.is_idle {
                    "idle"
                } else {
                    "running"
                };
                Some(TeammateRosterEntry {
                    name: data.identity.agent_name.clone(),
                    agent_type: snapshot.metadata_str("agent_type").map(str::to_string),
                    description: snapshot.metadata_str("description").map(str::to_string),
                    status: status.to_string(),
                    current_task: snapshot.metadata_str("current_task").map(str::to_string),
                    last_result: snapshot
                        .metadata_str("last_result")
                        .map(str::to_string)
                        .or_else(|| snapshot.last_progress.clone()),
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
        entries
    }

    fn close_session(&self, session_id: &str) {
        let _dispatch = self
            .dispatch_lock
            .lock()
            .expect("teammate dispatch lock poisoned");
        self.lifecycle.close_session(session_id);
    }

    async fn delete_team(&self, team_name: &str) -> Result<(), String> {
        let active: Vec<String> = self
            .registry
            .snapshots()
            .into_iter()
            .filter_map(|snap| match &snap.data {
                TaskData::InProcessTeammate(data)
                    if data.identity.team_name == team_name && !snap.status.is_terminal() =>
                {
                    Some(data.identity.agent_name.clone())
                }
                _ => None,
            })
            .collect();
        if !active.is_empty() {
            return Err(format!(
                "cannot delete team `{team_name}` while teammates are still active: {}",
                active.join(", ")
            ));
        }

        let team_dir = rebon_tool::team_files::team_dir(team_name);
        let tasks_dir = rebon_tool::tasks::tasks_dir(&sanitize_team_name(team_name));
        let _ = std::fs::remove_dir_all(team_dir);
        let _ = std::fs::remove_dir_all(tasks_dir);
        if rebon_tool::current_team_name().as_deref() == Some(team_name) {
            clear_current_team_name();
        }
        Ok(())
    }
}

impl InProcessTeamManager {
    fn find_team_teammate_snapshot(
        &self,
        team_name: &str,
        recipient: &str,
    ) -> Option<TaskSnapshot> {
        self.registry
            .snapshots()
            .into_iter()
            .find(|snapshot| match &snapshot.data {
                TaskData::InProcessTeammate(data) => {
                    data.identity.team_name == team_name
                        && data.identity.agent_name.eq_ignore_ascii_case(recipient)
                }
                _ => false,
            })
    }

    fn find_teammate_task_id(&self, team_name: &str, recipient: &str) -> Option<TaskId> {
        self.registry
            .snapshots()
            .into_iter()
            .find_map(|snap| match &snap.data {
                TaskData::InProcessTeammate(data)
                    if data.identity.team_name == team_name
                        && data.identity.agent_name.eq_ignore_ascii_case(recipient)
                        && !snap.status.is_terminal() =>
                {
                    Some(snap.id)
                }
                _ => None,
            })
    }
}

fn teammate_conversation_messages(snapshot: &TaskSnapshot) -> Vec<rebon_api::Message> {
    let TaskData::InProcessTeammate(data) = &snapshot.data else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for entry in &data.transcript {
        let (role, text) = match entry {
            LocalAgentTranscriptEntry::User { text } => (rebon_api::Role::User, text),
            LocalAgentTranscriptEntry::Assistant { text } => (rebon_api::Role::Assistant, text),
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let message = match role {
            rebon_api::Role::User => rebon_api::Message::user_text(text.clone()),
            rebon_api::Role::Assistant => rebon_api::Message::assistant_text(text.clone()),
            rebon_api::Role::System => continue,
        };
        messages.push(message);
    }
    if messages.len() > 64 {
        messages.drain(..messages.len() - 64);
    }
    messages
}

fn message_text(message: &rebon_api::Message) -> Option<&str> {
    message.content.iter().find_map(|block| match block {
        rebon_api::ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    })
}

fn compact_handoff_text(text: &str) -> String {
    const MAX_CHARS: usize = 1_200;
    let text = text.trim();
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let mut compact = text.chars().take(MAX_CHARS).collect::<String>();
    compact.push('…');
    compact
}

fn complete_teammate_request(
    waiters: &Arc<Mutex<HashMap<String, oneshot::Sender<TeammateHandoff>>>>,
    request_id: Option<&str>,
    handoff: TeammateHandoff,
) {
    let Some(request_id) = request_id else {
        return;
    };
    if let Some(sender) = waiters
        .lock()
        .expect("teammate waiter registry poisoned")
        .remove(request_id)
    {
        let _ = sender.send(handoff);
    }
}

fn fail_teammate_waiters(
    waiters: &Arc<Mutex<HashMap<String, oneshot::Sender<TeammateHandoff>>>>,
    task_id: &TaskId,
    error: &str,
) {
    let prefix = format!("{task_id}:");
    let pending = {
        let mut waiters = waiters.lock().expect("teammate waiter registry poisoned");
        let ids = waiters
            .keys()
            .filter(|request_id| request_id.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|request_id| waiters.remove(&request_id))
            .collect::<Vec<_>>()
    };
    for sender in pending {
        let _ = sender.send(TeammateHandoff {
            status: "failed".into(),
            summary: compact_handoff_text(error),
            error: Some(error.to_string()),
        });
    }
}

fn record_teammate_handoff(registry: &TaskRegistry, task_id: &TaskId, handoff: &TeammateHandoff) {
    registry.update(task_id, |snapshot| {
        if let Some(metadata) = snapshot.metadata.as_object_mut() {
            metadata.insert(
                "last_result".into(),
                serde_json::Value::String(handoff.summary.clone()),
            );
            metadata.remove("current_task");
        }
    });
}

#[allow(clippy::too_many_arguments)]
/// What one teammate turn needs to run.
struct TeammateTurn {
    session: Arc<rebon_api::SessionHandle>,
    turn_user_message: rebon_api::Message,
    worker_spec: WorkerSpec,
}

/// Assemble the worker for one teammate turn.
///
/// The teammate's session is memoised in `teammate_session`: it is forked
/// once and reused for every turn, so a teammate keeps one provider
/// connection for its whole life. The permission stack mirrors an
/// AgentTool-spawned sub-agent's exactly -- see `spawn_inner` for why.
#[allow(clippy::too_many_arguments)]
fn build_teammate_turn(
    engine: &Arc<Engine>,
    registry: &TaskRegistry,
    task_id: &TaskId,
    identity: &TeammateIdentity,
    spec: &TeammateSpawnSpec,
    base_filter: &Option<SharedToolFilter>,
    resolved_runtime: &rebon_agent_core::model_router::ResolvedModelRuntime,
    teammate_session: &mut Option<Arc<rebon_api::SessionHandle>>,
    conversation: &Vec<rebon_api::Message>,
    prompt: String,
    permission_mode: &String,
    model: String,
    reasoning_effort: Option<rebon_types::ReasoningEffort>,
    prompt_already_in_transcript: bool,
) -> TeammateTurn {
    let _ = set_in_process_teammate_current_task(registry, task_id, &prompt);
    persist_teammate_activity(&identity.team_name, &identity.agent_name, true);
    let plan_mode = permission_mode == PLAN_PERMISSION_MODE;
    // The transcript echoes the raw prompt; the plan-mode
    // preamble is worker-facing plumbing and stays out of the UI.
    if !prompt_already_in_transcript {
        push_in_process_teammate_turn_prompt(registry, task_id, &prompt);
    }
    let effective_prompt = if plan_mode {
        format!(
        "Work in plan mode. Analyze the task and propose the plan only. Do not edit files yet.\n\n{}",
        prompt
    )
    } else {
        prompt
    };
    let turn_user_message = rebon_api::Message::user_text(effective_prompt.clone());
    let mut turn_messages = conversation.clone();
    turn_messages.push(turn_user_message.clone());
    // Mid-turn teammate_mailbox drain: gives the running worker
    // visibility into inter-agent traffic without waiting for
    // this iteration of `run_teammate_loop` to end. Structured
    // protocol messages stay filtered inside
    // `drain_teammate_mailbox_for`, so `try_read_mailbox_prompt`
    // remains the authoritative handler for plan_approval_response
    // (it needs to clear `awaiting_plan_approval` and format a
    // fresh worker prompt).
    let attachment_poller: std::sync::Arc<dyn rebon_core::query::AttachmentPoller> =
        std::sync::Arc::new(rebon_plugin_tasks::TeammateMailboxPoller::new(
            rebon_core::attachment_seat::MailboxIdentity {
                team_name: identity.team_name.clone(),
                agent_name: identity.agent_name.clone(),
            },
        ));
    // Same borrowed-parent/forked-child shape as the sub-agent
    // spawner: the teammate gets its own session when the client
    // can isolate one, and never resets the shared one when it
    // cannot. See `EngineSubAgentSpawner::spawn_inner`.
    let session = teammate_session
        .get_or_insert_with(|| {
            rebon_api::SessionHandle::borrowed(resolved_runtime.client.clone())
                .fork_for_sub_agent(None)
        })
        .clone();
    // Same permission stack as an AgentTool-spawned sub-agent
    // (see `EngineSubAgentSpawner::spawn_inner`): regular tools
    // auto-approve; sensitive commands check the background
    // probe. Teammates register with `is_backgrounded: true` and
    // nothing flips it today, so in practice sensitive commands
    // are denied cleanly (fail-closed) rather than delegated to
    // an interactive prompt — with the same single exception as
    // the spawner: a file deletion may float to the frontend as a
    // permission ask when the parent runtime has a prompt
    // surface. The probe + delegate shape is kept deliberately, so
    // full interactive approval starts working if foregrounding
    // ever clears the backgrounded flag.
    // Keeps the parent's stored permission rules (allow_always,
    // settings allow/deny lists) in front of the prompt broker so
    // they stay effective inside the teammate.
    let sensitive_permission_broker = spec
        .permission_broker
        .as_ref()
        .and_then(|broker| {
            rebon_core::permission::sub_agent_sensitive_permission_broker_from(
                broker,
                rebon_tool::WorkflowNesting::at(spec.workflow_nesting_depth).is_within_workflow(),
            )
        })
        .unwrap_or_else(|| engine.permission_broker().clone());
    let background_probe: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = {
        let registry = registry.clone();
        let task_id = task_id.clone();
        std::sync::Arc::new(move || {
            registry
                .snapshot(&task_id)
                .is_none_or(|snap| snap.is_backgrounded)
        })
    };
    let worker_permission_broker = std::sync::Arc::new(
        AutoApproveExceptSensitivePermissionBroker::with_background_probe(
            sensitive_permission_broker,
            background_probe,
        )
        .with_background_deletion_ask(!spec.permission_prompts_unavailable),
    );
    let escalation_client = registry.escalation_registry().worker_client(
        identity.agent_id.clone(),
        Some(format!("@{}", identity.agent_name)),
    );
    let worker_spec = WorkerSpec {
        messages: turn_messages,
        model,
        system: spec.system.clone(),
        tool_filter: Some(teammate_worker_filter(plan_mode, base_filter.as_ref())),
        max_iterations: rebon_tool::DEFAULT_SUB_AGENT_MAX_ITERATIONS,
        max_tokens: crate::runtime::worker::worker_max_tokens_for_client(
            resolved_runtime.client.as_ref(),
        ),
        tool_context: {
            // Inherit the parent's working directory so teammate
            // file/shell tools do not fall back to the process
            // launch directory.
            let mut tool_context = ToolContext::new()
                .with_permission_broker(worker_permission_broker)
                .with_worker_escalation_client(escalation_client)
                .with_team_identity(TeamIdentityContext {
                    agent_id: identity.agent_id.clone(),
                    agent_name: identity.agent_name.clone(),
                    team_name: identity.team_name.clone(),
                    permission_mode: Some(permission_mode.clone()),
                })
                .with_agent_id(identity.agent_id.clone())
                .with_task_list_id(spec.team_name.clone())
                .with_workflow_nesting_depth(spec.workflow_nesting_depth)
                .with_permission_prompts_unavailable(spec.permission_prompts_unavailable);
            if let Some(cwd) = spec.cwd.clone() {
                tool_context = tool_context.with_cwd(cwd);
            }
            if !spec.additional_working_directories.is_empty() {
                tool_context = tool_context.with_additional_working_directories(
                    spec.additional_working_directories.clone(),
                );
            }
            tool_context
        },
        turn_hook: None,
        // Teammates ask nobody, which is what they did before the policy
        // seat existed. Reaching them means carrying the handle through
        // `TeamManagerRequest` and a fifteenth argument to this loop, and
        // the loop's argument list is already past what the repo allows;
        // it wants the parameter struct first. Nothing regresses here.
        policy: rebon_core::policy_seat::PolicySources::default(),
        attachment_poller: Some(rebon_core::query::AttachmentPollerBinding::new(
            attachment_poller,
            identity.agent_id.clone(),
            task_id.to_string(),
        )),
        reasoning_effort,
        prune_level: session.context_prune_handle(),
        execution_policy: None,
        capability_context: None,
        tools_hash: None,
        schema_hash: None,
        tools_token_estimate: None,
        cache_trace_context: None,
    };
    TeammateTurn {
        session,
        turn_user_message,
        worker_spec,
    }
}

/// Record one finished teammate turn.
///
/// The handoff is what the requester is waiting on, so it is built from the
/// worker's result and delivered before the transcript is trimmed. The
/// conversation keeps at most 64 messages: a teammate is long-lived and this
/// is the only bound on its context.
#[allow(clippy::too_many_arguments)]
fn record_teammate_turn(
    registry: &TaskRegistry,
    task_id: &TaskId,
    identity: &TeammateIdentity,
    waiters: &Arc<Mutex<HashMap<String, oneshot::Sender<TeammateHandoff>>>>,
    conversation: &mut Vec<rebon_api::Message>,
    turn_user_message: rebon_api::Message,
    request_id: Option<String>,
    result: &crate::runtime::worker::WorkerResult,
) {
    let last_progress = if let Some(error) = result.error.as_ref() {
        Some(format!("error: {error}"))
    } else if result.final_text.trim().is_empty() {
        // Read the live snapshot, not the one captured at the top
        // of the loop — the pump has been writing fresher
        // progress lines all turn.
        registry
            .snapshot(task_id)
            .and_then(|snap| snap.last_progress)
    } else {
        Some(result.final_text.clone())
    };

    let handoff_status = match result.status {
        WorkerStatus::Completed => "completed",
        WorkerStatus::Failed => "failed",
        WorkerStatus::Cancelled => "cancelled",
        WorkerStatus::Running => "running",
    };
    let handoff_summary = if result.final_text.trim().is_empty() {
        last_progress
            .as_deref()
            .or(result.error.as_deref())
            .unwrap_or("teammate turn finished without a textual result")
    } else {
        result.final_text.as_str()
    };
    let handoff = TeammateHandoff {
        status: handoff_status.into(),
        summary: compact_handoff_text(handoff_summary),
        error: result.error.clone(),
    };
    conversation.push(turn_user_message);
    if !result.final_text.trim().is_empty() {
        conversation.push(rebon_api::Message::assistant_text(
            result.final_text.clone(),
        ));
    }
    if conversation.len() > 64 {
        conversation.drain(..conversation.len() - 64);
    }
    record_teammate_handoff(registry, task_id, &handoff);
    complete_teammate_request(waiters, request_id.as_deref(), handoff);

    match result.status {
        WorkerStatus::Completed | WorkerStatus::Failed | WorkerStatus::Cancelled => {
            let _ = mark_in_process_teammate_idle(registry, task_id, last_progress.clone());
            persist_teammate_activity(&identity.team_name, &identity.agent_name, false);
            let idle_reason = match result.status {
                WorkerStatus::Failed => "failed",
                WorkerStatus::Cancelled => "interrupted",
                _ => "available",
            };
            send_idle_notification(
                &identity.team_name,
                &identity.agent_name,
                idle_reason,
                last_progress.as_deref(),
                result.error.as_deref(),
            );
        }
        WorkerStatus::Running => {}
    }
}

/// Wind the teammate down after its loop ends.
///
/// A failed teammate is restartable, so its waiters are told that and the
/// team registration is left alone. Any other exit is final: the waiters are
/// failed, the teammate's tasks are cleaned up, it is removed from the team,
/// and the leader is told it has gone.
fn finish_teammate_loop(
    registry: &TaskRegistry,
    task_id: &TaskId,
    identity: &TeammateIdentity,
    spec: &TeammateSpawnSpec,
    waiters: &Arc<Mutex<HashMap<String, oneshot::Sender<TeammateHandoff>>>>,
) {
    let restartable_failure = registry
        .snapshot(task_id)
        .is_some_and(|snapshot| snapshot.status == TaskStatus::Failed);
    if restartable_failure {
        fail_teammate_waiters(
            waiters,
            task_id,
            "teammate runtime failed before completing the queued request",
        );
        return;
    }

    fail_teammate_waiters(
        waiters,
        task_id,
        "teammate stopped before completing the queued request",
    );
    if is_session_default_team_name(&identity.team_name) {
        if !session_has_other_teammates(registry, &identity.parent_session_id, task_id) {
            if let Err(error) = remove_session_default_team(&identity.parent_session_id) {
                tracing::warn!(
                    session_id = %identity.parent_session_id,
                    %error,
                    "failed to remove session default team after teammate shutdown"
                );
            }
        }
        return;
    }
    cleanup_teammate_created_tasks(&spec.team_name, &identity.agent_id);

    let _ = remove_member_by_agent_id(&identity.team_name, &identity.agent_id);

    // Notify the leader that this teammate has fully exited.
    // Emits the `teammate_terminated` inbox message used by the team UI.
    let terminated_payload = serde_json::json!({
        "type": "teammate_terminated",
        "message": format!("{} has shut down.", identity.agent_name),
    });
    let _ = write_mailbox_message(
        &identity.team_name,
        TEAM_LEAD_NAME,
        TeamMailboxMessage {
            from: identity.agent_name.clone(),
            text: terminated_payload.to_string(),
            timestamp: now_wall_ms().to_string(),
            read: false,
            color: None,
            summary: Some(format!("{} terminated", identity.agent_name)),
        },
    );
}

async fn run_teammate_loop(
    engine: Arc<Engine>,
    _client: Arc<dyn ModelClient>,
    registry: TaskRegistry,
    cancel: PromptCancel,
    task_id: TaskId,
    identity: TeammateIdentity,
    spec: TeammateSpawnSpec,
    initial_request: Option<TeammateRequest>,
    _default_model: String,
    _model_config: SubAgentModelConfig,
    _model_profiles: ModelProfileMap,
    model_router: Arc<dyn AgentModelRouter>,
    base_filter: Option<SharedToolFilter>,
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<TeammateHandoff>>>>,
) {
    let mut next_request = initial_request;
    let mut conversation = registry
        .snapshot(&task_id)
        .map(|snapshot| teammate_conversation_messages(&snapshot))
        .unwrap_or_default();
    let mut teammate_session: Option<Arc<rebon_api::SessionHandle>> = None;
    let configured_reasoning_effort = spec.effort.as_deref().and_then(parse_reasoning_effort);
    let Some(runtime_wake) = registry.task_waker(&task_id) else {
        return;
    };
    let mailbox_wake = mailbox_notification(&identity.team_name, &identity.agent_name);
    let task_board_wake = task_list_notification(&spec.team_name);

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let runtime_notified = runtime_wake.notified();
        let mailbox_notified = mailbox_wake.notified();
        let task_board_notified = task_board_wake.notified();
        tokio::pin!(runtime_notified, mailbox_notified, task_board_notified);
        let _ = runtime_notified.as_mut().enable();
        let _ = mailbox_notified.as_mut().enable();
        let _ = task_board_notified.as_mut().enable();

        let Some(snapshot) = registry.snapshot(&task_id) else {
            break;
        };
        if snapshot.status.is_terminal() {
            break;
        }
        let mut shutdown_requested = false;
        let mut model = spec.model.clone().unwrap_or_else(|| _default_model.clone());
        let mut model_profile = spec.model_profile.clone();
        let mut permission_mode = String::from("default");
        let mut awaiting_plan_approval = false;
        if let TaskData::InProcessTeammate(data) = &snapshot.data {
            shutdown_requested = data.shutdown_requested;
            if let Some(task_model) = data.model.clone() {
                model = task_model;
            }
            if let Some(task_profile) = data.model_profile.clone() {
                model_profile = Some(task_profile);
            }
            permission_mode = data.permission_mode.clone();
            awaiting_plan_approval = data.awaiting_plan_approval;
        }
        let model_request = if spec.model.is_some() {
            spec.model.clone()
        } else if model == _default_model {
            None
        } else {
            Some(model.clone())
        };
        let route_request = ModelRouteRequest {
            provider: spec.provider.clone(),
            model: model_request,
            model_profile,
            agent_type: spec.agent_type.clone(),
            category: None,
            reasoning_effort: configured_reasoning_effort,
        };
        let resolved_runtime = match model_router.resolve(route_request).await {
            Ok(runtime) => runtime,
            Err(err) => {
                if cancel.is_cancelled() || registry.snapshot(&task_id).is_none() {
                    break;
                }
                let err = err.to_string();
                // Routing failure exits the loop — finish with a
                // terminal status so the task doesn't linger as a
                // Running/idle orphan in the background-agents list.
                let _ = finish_in_process_teammate(
                    &registry,
                    &task_id,
                    TaskStatus::Failed,
                    Some(format!("teammate provider/model routing failed: {err}")),
                    Some(err.clone()),
                );
                persist_teammate_activity(&identity.team_name, &identity.agent_name, false);
                send_idle_notification(
                    &identity.team_name,
                    &identity.agent_name,
                    "failed",
                    None,
                    Some(&err),
                );
                fail_teammate_waiters(&waiters, &task_id, &err);
                break;
            }
        };
        if cancel.is_cancelled() || registry.snapshot(&task_id).is_none() {
            break;
        }
        let model = resolved_runtime.model.clone();
        let reasoning_effort = resolved_runtime.reasoning_effort;
        if shutdown_requested && next_request.is_none() {
            let _ = finish_in_process_teammate(
                &registry,
                &task_id,
                TaskStatus::Completed,
                snapshot.last_progress.clone(),
                None,
            );
            persist_teammate_activity(&identity.team_name, &identity.agent_name, false);
            send_idle_notification(
                &identity.team_name,
                &identity.agent_name,
                "available",
                snapshot.last_progress.as_deref(),
                None,
            );
            break;
        }

        // Prompts popped from `pending_user_messages` were already
        // echoed into the transcript by
        // `inject_user_message_to_teammate`; don't push them twice.
        let mut prompt_already_in_transcript = false;
        let (prompt, request_id) = if let Some(request) = next_request.take() {
            (request.message, Some(request.request_id))
        } else if let Some(request) = take_pending_teammate_request(&registry, &task_id) {
            prompt_already_in_transcript = true;
            (request.message, Some(request.request_id))
        } else if let Some(mailbox_message) = try_read_mailbox_prompt(
            &registry,
            &task_id,
            &identity.team_name,
            &identity.agent_name,
            &snapshot,
        )
        .await
        {
            (mailbox_message, None)
        } else if !awaiting_plan_approval && !is_session_default_team_name(&identity.team_name) {
            if let Some(task_prompt) =
                try_claim_next_task(&spec.team_name, &identity.agent_name, &identity.agent_id).await
            {
                (task_prompt, None)
            } else {
                tokio::select! {
                    _ = cancel.notified() => break,
                    _ = &mut runtime_notified => continue,
                    _ = &mut mailbox_notified => continue,
                    _ = &mut task_board_notified => continue,
                }
            }
        } else {
            tokio::select! {
                _ = cancel.notified() => break,
                _ = &mut runtime_notified => continue,
                _ = &mut mailbox_notified => continue,
                _ = &mut task_board_notified => continue,
            }
        };

        if prompt_already_in_transcript
            && conversation.last().is_some_and(|message| {
                message.role == rebon_api::Role::User
                    && message_text(message) == Some(prompt.as_str())
            })
        {
            conversation.pop();
        }
        let Some(turn) = registry.begin_task_turn(&task_id) else {
            break;
        };
        if !mark_in_process_teammate_running(&registry, &task_id) {
            break;
        }
        let TeammateTurn {
            session,
            turn_user_message,
            worker_spec,
        } = build_teammate_turn(
            &engine,
            &registry,
            &task_id,
            &identity,
            &spec,
            &base_filter,
            &resolved_runtime,
            &mut teammate_session,
            &conversation,
            prompt,
            &permission_mode,
            model,
            reasoning_effort,
            prompt_already_in_transcript,
        );

        let result = match spawn_worker(engine.clone(), session, worker_spec, cancel.clone()) {
            Ok(handle) => drive_in_process_teammate_worker_turn(&registry, &turn, handle).await,
            Err(err) => {
                if cancel.is_cancelled() || registry.snapshot(&task_id).is_none() {
                    break;
                }
                let error = format!("teammate worker spawn failed: {err}");
                let handoff = TeammateHandoff {
                    status: "failed".into(),
                    summary: compact_handoff_text(&error),
                    error: Some(error.clone()),
                };
                let _ = mark_in_process_teammate_idle(&registry, &task_id, Some(error.clone()));
                record_teammate_handoff(&registry, &task_id, &handoff);
                complete_teammate_request(&waiters, request_id.as_deref(), handoff);
                let _ = registry.finish_task_turn(&turn);
                persist_teammate_activity(&identity.team_name, &identity.agent_name, false);
                send_idle_notification(
                    &identity.team_name,
                    &identity.agent_name,
                    "failed",
                    None,
                    Some(&error),
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        if cancel.is_cancelled() {
            break;
        }

        record_teammate_turn(
            &registry,
            &task_id,
            &identity,
            &waiters,
            &mut conversation,
            turn_user_message,
            request_id,
            &result,
        );
        let _ = registry.finish_task_turn(&turn);

        if let Some(updated) = registry.snapshot(&task_id) {
            if let TaskData::InProcessTeammate(data) = &updated.data {
                if data.shutdown_requested {
                    let _ = finish_in_process_teammate(
                        &registry,
                        &task_id,
                        TaskStatus::Completed,
                        updated.last_progress.clone(),
                        None,
                    );
                    persist_teammate_activity(&identity.team_name, &identity.agent_name, false);
                    send_idle_notification(
                        &identity.team_name,
                        &identity.agent_name,
                        "available",
                        updated.last_progress.as_deref(),
                        None,
                    );
                    break;
                }
            }
        }
    }

    finish_teammate_loop(&registry, &task_id, &identity, &spec, &waiters);
}

fn session_has_other_teammates(
    registry: &TaskRegistry,
    parent_session_id: &str,
    current_task_id: &TaskId,
) -> bool {
    registry.snapshots().into_iter().any(|snapshot| {
        snapshot.id != *current_task_id
            && !snapshot.status.is_terminal()
            && matches!(
                snapshot.data,
                TaskData::InProcessTeammate(data)
                    if data.identity.parent_session_id == parent_session_id
            )
    })
}

fn cleanup_teammate_created_tasks(task_list_id: &str, agent_id: &str) {
    match rebon_tool::tasks::cleanup_in_progress_tasks_for_agent(
        task_list_id,
        agent_id,
        rebon_tool::tasks::TaskListStatus::Pending,
    ) {
        Ok(count) if count > 0 => tracing::debug!(
            agent_id = %agent_id,
            task_list_id = %task_list_id,
            count,
            "cleaned up in-progress tasks created by terminal teammate"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            agent_id = %agent_id,
            task_list_id = %task_list_id,
            error = %err,
            "failed to clean up in-progress tasks created by terminal teammate"
        ),
    }
}

fn persist_teammate_activity(team_name: &str, agent_name: &str, is_active: bool) {
    if let Err(error) = set_team_member_active(team_name, agent_name, is_active) {
        tracing::warn!(
            team = team_name,
            agent = agent_name,
            error = %error,
            "failed to persist teammate activity"
        );
    }
}

/// Write an idle notification to the leader's mailbox so the team lead
/// knows the teammate has finished its current turn.
fn send_idle_notification(
    team_name: &str,
    agent_name: &str,
    idle_reason: &str,
    summary: Option<&str>,
    failure_reason: Option<&str>,
) {
    let notification = serde_json::json!({
        "type": "idle_notification",
        "from": agent_name,
        "timestamp": now_wall_ms().to_string(),
        "idleReason": idle_reason,
        "summary": summary,
        "failureReason": failure_reason,
    });
    let summary_text = if idle_reason == "failed" {
        Some(format!(
            "{} failed: {}",
            agent_name,
            failure_reason.unwrap_or("unknown error"),
        ))
    } else {
        Some(format!(
            "{} idle{}",
            agent_name,
            summary
                .map(|s| format!(" · {}", truncate_summary(s, 60)))
                .unwrap_or_default(),
        ))
    };
    let _ = write_mailbox_message(
        team_name,
        TEAM_LEAD_NAME,
        TeamMailboxMessage {
            from: agent_name.to_string(),
            text: notification.to_string(),
            timestamp: now_wall_ms().to_string(),
            read: false,
            color: None,
            summary: summary_text,
        },
    );
}

fn truncate_summary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s
            .char_indices()
            .take_while(|(i, _)| *i < max)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(max);
        &s[..end]
    }
}

async fn try_claim_next_task(
    task_list_id: &str,
    agent_name: &str,
    agent_id: &str,
) -> Option<String> {
    let tasks = list_tasks(task_list_id).ok()?;
    let unresolved: std::collections::HashSet<String> = tasks
        .iter()
        .filter(|task| task.status != TaskListStatus::Completed)
        .map(|task| task.id.clone())
        .collect();

    let owned = tasks.iter().find(|task| {
        task.status == TaskListStatus::Pending
            && task.owner.as_deref() == Some(agent_name)
            && task.blocked_by.iter().all(|id| !unresolved.contains(id))
    });
    let selected = if let Some(task) = owned {
        task
    } else {
        tasks.iter().find(|task| {
            task.status == TaskListStatus::Pending
                && task.owner.is_none()
                && task.blocked_by.iter().all(|id| !unresolved.contains(id))
        })?
    };

    let owner = selected
        .owner
        .clone()
        .or_else(|| Some(agent_name.to_string()));
    let mut metadata = selected.metadata.clone().unwrap_or_default();
    metadata.insert(
        String::from("agentId"),
        serde_json::Value::String(agent_id.to_string()),
    );
    let _ = update_task(
        task_list_id,
        &selected.id,
        TaskPatch {
            owner: Some(owner),
            status: Some(TaskListStatus::InProgress),
            metadata: Some(Some(metadata)),
            ..TaskPatch::default()
        },
    )
    .ok()?;

    let mut prompt = format!("Complete task #{}:\n\n{}", selected.id, selected.subject);
    if !selected.description.trim().is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&selected.description);
    }
    Some(prompt)
}

async fn try_read_mailbox_prompt(
    registry: &TaskRegistry,
    task_id: &TaskId,
    team_name: &str,
    agent_name: &str,
    snapshot: &TaskSnapshot,
) -> Option<String> {
    let unread = drain_unread_mailbox(team_name, agent_name).ok()?;
    if let Some(message) = unread.into_iter().next() {
        if let Ok(response) = serde_json::from_str::<PlanApprovalResponse>(&message.text) {
            if response.kind == "plan_approval_response" {
                let _ = set_in_process_teammate_awaiting_plan_approval(registry, task_id, false);
                if response.approved {
                    let next_mode = response
                        .permission_mode
                        .clone()
                        .unwrap_or_else(|| "default".into());
                    let _ = set_in_process_teammate_permission_mode(
                        registry,
                        task_id,
                        next_mode.clone(),
                    );
                    let _ = set_team_member_mode(team_name, agent_name, &next_mode);
                    let prompt = match &snapshot.data {
                        TaskData::InProcessTeammate(data) => format!(
                            "Your plan approval request {} was approved. Proceed with implementation.\n\nOriginal task:\n{}",
                            response.request_id, data.prompt
                        ),
                        _ => format!(
                            "Your plan approval request {} was approved. Proceed with implementation.",
                            response.request_id
                        ),
                    };
                    return Some(prompt);
                }

                let feedback = response
                    .feedback
                    .unwrap_or_else(|| "Please revise your plan.".into());
                let prompt = match &snapshot.data {
                    TaskData::InProcessTeammate(data) => format!(
                        "Your plan approval request {} was rejected. Revise the plan and call ExitPlanMode again.\n\nFeedback: {}\n\nOriginal task:\n{}",
                        response.request_id, feedback, data.prompt
                    ),
                    _ => format!(
                        "Your plan approval request {} was rejected. Revise the plan and call ExitPlanMode again.\n\nFeedback: {}",
                        response.request_id, feedback
                    ),
                };
                return Some(prompt);
            }
        }
        return Some(message.text);
    }
    None
}

struct SessionTeamManagerEntry {
    registry: Arc<TaskRegistry>,
    manager: Arc<InProcessTeamManager>,
}

/// Multi-session adapter that resolves one in-process manager from the exact
/// session task-registry seat. It owns no registry fallback: disabled or
/// disposed seats make new operations unavailable.
pub struct SessionTaskTeamManager {
    engine: Arc<Engine>,
    client: Arc<dyn ModelClient>,
    registry_resolver: rebon_plugin_tasks::TaskRegistryResolver,
    default_model: String,
    model_config: SubAgentModelConfig,
    model_profiles: ModelProfileMap,
    model_router: Option<Arc<dyn AgentModelRouter>>,
    base_filter: Option<SharedToolFilter>,
    managers: Mutex<HashMap<String, SessionTeamManagerEntry>>,
}

impl SessionTaskTeamManager {
    pub fn new(
        engine: Arc<Engine>,
        client: Arc<dyn ModelClient>,
        registry_resolver: rebon_plugin_tasks::TaskRegistryResolver,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            engine,
            client,
            registry_resolver,
            default_model: default_model.into(),
            model_config: SubAgentModelConfig::default(),
            model_profiles: ModelProfileMap::default(),
            model_router: None,
            base_filter: None,
            managers: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_shared_base_filter(mut self, filter: SharedToolFilter) -> Self {
        self.base_filter = Some(filter);
        self
    }

    pub fn with_model_config(mut self, config: SubAgentModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub fn with_model_profiles(mut self, profiles: ModelProfileMap) -> Self {
        self.model_profiles = profiles;
        self
    }

    pub fn with_model_router(mut self, router: Arc<dyn AgentModelRouter>) -> Self {
        self.model_router = Some(router);
        self
    }

    fn manager_for(&self, session_id: &str) -> Result<Arc<InProcessTeamManager>, String> {
        if session_id.trim().is_empty() {
            return Err("team operations require a session id".into());
        }
        // Resolve on every operation so plugin disable and scope disposal are
        // observed even when a manager for this session already exists.
        let registry = self.registry_resolver.resolve(session_id)?;
        {
            let managers = self
                .managers
                .lock()
                .expect("session team manager table poisoned");
            if let Some(entry) = managers.get(session_id) {
                if Arc::ptr_eq(&entry.registry, &registry) {
                    return Ok(entry.manager.clone());
                }
            }
        }
        let mut manager = InProcessTeamManager::new(
            self.engine.clone(),
            self.client.clone(),
            registry.as_ref().clone(),
            self.default_model.clone(),
        )
        .with_model_config(self.model_config.clone())
        .with_model_profiles(self.model_profiles.clone());
        if let Some(router) = &self.model_router {
            manager = manager.with_model_router(router.clone());
        }
        if let Some(filter) = &self.base_filter {
            manager = manager.with_shared_base_filter(filter.clone());
        }
        let manager = Arc::new(manager);
        let mut managers = self
            .managers
            .lock()
            .expect("session team manager table poisoned");
        if let Some(entry) = managers.get(session_id) {
            if Arc::ptr_eq(&entry.registry, &registry) {
                return Ok(entry.manager.clone());
            }
        }
        managers.insert(
            session_id.to_string(),
            SessionTeamManagerEntry {
                registry,
                manager: manager.clone(),
            },
        );
        Ok(manager)
    }

    fn session_for_team(team_name: &str) -> Result<String, String> {
        read_team_file(team_name)
            .map_err(|error| error.to_string())?
            .and_then(|team| team.lead_session_id)
            .filter(|session_id| !session_id.trim().is_empty())
            .ok_or_else(|| format!("team `{team_name}` has no owning session"))
    }
}

#[async_trait]
impl TeamManager for SessionTaskTeamManager {
    async fn spawn_teammate(&self, spec: TeammateSpawnSpec) -> Result<TeammateSpawnResult, String> {
        self.manager_for(&spec.parent_session_id)?
            .spawn_teammate(spec)
            .await
    }

    fn team_name_for_session(&self, session_id: &str) -> Option<String> {
        self.manager_for(session_id)
            .ok()
            .and_then(|manager| manager.team_name_for_session(session_id))
    }

    fn teammate_roster(&self, session_id: &str) -> Vec<TeammateRosterEntry> {
        self.manager_for(session_id)
            .map(|manager| manager.teammate_roster(session_id))
            .unwrap_or_default()
    }

    async fn send_message(
        &self,
        team_name: &str,
        recipient: &str,
        message: String,
    ) -> Result<(), ToolErrorPresentation> {
        let session_id = Self::session_for_team(team_name).map_err(|error| {
            ToolErrorPresentation::new("team_session_unavailable", error.clone(), error)
        })?;
        self.manager_for(&session_id)
            .map_err(|error| {
                ToolErrorPresentation::new("task_registry_unavailable", error.clone(), error)
            })?
            .send_message(team_name, recipient, message)
            .await
    }

    async fn send_message_to_task(
        &self,
        task_id: &str,
        _message: String,
    ) -> Result<(), ToolErrorPresentation> {
        Err(ToolErrorPresentation::new(
            "task_session_required",
            format!("Agent \"{task_id}\" is not available."),
            "Routing a task id through a multi-session team manager requires an exact session; use the task runtime controller",
        ))
    }

    async fn request_shutdown(
        &self,
        team_name: &str,
        recipient: &str,
        reason: Option<String>,
    ) -> Result<String, String> {
        let session_id = Self::session_for_team(team_name)?;
        self.manager_for(&session_id)?
            .request_shutdown(team_name, recipient, reason)
            .await
    }

    async fn request_plan_approval(
        &self,
        team_name: &str,
        agent_name: &str,
        plan_content: String,
    ) -> Result<String, String> {
        let session_id = Self::session_for_team(team_name)?;
        self.manager_for(&session_id)?
            .request_plan_approval(team_name, agent_name, plan_content)
            .await
    }

    fn close_session(&self, session_id: &str) {
        if let Some(entry) = self
            .managers
            .lock()
            .expect("session team manager table poisoned")
            .remove(session_id)
        {
            entry.manager.close_session(session_id);
        }
    }

    async fn delete_team(&self, team_name: &str) -> Result<(), String> {
        let session_id = Self::session_for_team(team_name)?;
        self.manager_for(&session_id)?.delete_team(team_name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{
        events::{ContentBlockDelta, ContentBlockStart, MessageDeltaFields},
        MockModelClient, StopReason, StreamEvent, Usage,
    };
    use rebon_core::Engine;
    use rebon_tool::team_files::{write_team_file, TeamFile};

    fn env_lock() -> crate::runtime::TestEnvLock {
        crate::runtime::test_config_env_lock()
    }

    struct TestHome {
        _guard: crate::runtime::TestEnvLock,
        _dir: tempfile::TempDir,
        old_config_dir: Option<String>,
        old_team_name: Option<String>,
        old_session_id: Option<String>,
    }

    impl TestHome {
        fn new(prefix: &str) -> Self {
            let guard = env_lock();
            let dir = tempfile::Builder::new()
                .prefix(&format!("rebon-team-manager-tests-{prefix}-"))
                .tempdir()
                .unwrap();
            let old_config_dir = std::env::var("REBON_CONFIG_DIR").ok();
            let old_team_name = std::env::var("REBON_TEAM_NAME").ok();
            let old_session_id = std::env::var("REBON_SESSION_ID").ok();
            std::env::set_var("REBON_CONFIG_DIR", dir.path());
            std::env::remove_var("REBON_TEAM_NAME");
            std::env::remove_var("REBON_SESSION_ID");
            Self {
                _guard: guard,
                _dir: dir,
                old_config_dir,
                old_team_name,
                old_session_id,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.old_config_dir.as_deref() {
                Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
            match self.old_team_name.as_deref() {
                Some(v) => std::env::set_var("REBON_TEAM_NAME", v),
                None => std::env::remove_var("REBON_TEAM_NAME"),
            }
            match self.old_session_id.as_deref() {
                Some(v) => std::env::set_var("REBON_SESSION_ID", v),
                None => std::env::remove_var("REBON_SESSION_ID"),
            }
        }
    }

    fn text_turn(id: &str, text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message_id: id.into(),
                model: "mock".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::TextDelta { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage::default(),
                },
            },
            StreamEvent::MessageStop,
        ]
    }

    fn write_test_team(team_name: &str) {
        write_team_file(
            team_name,
            &TeamFile {
                name: team_name.into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: format!("team-lead@{team_name}"),
                lead_session_id: None,
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();
    }

    fn test_spawn_spec(
        team_name: &str,
        name: &str,
        prompt: &str,
        parent_session_id: &str,
    ) -> TeammateSpawnSpec {
        TeammateSpawnSpec {
            team_name: team_name.into(),
            name: name.into(),
            prompt: prompt.into(),
            agent_type: Some("general-purpose".into()),
            model: Some("mock".into()),
            model_profile: None,
            provider: None,
            mode: Some("default".into()),
            effort: None,
            description: Some("Reusable test teammate".into()),
            system: None,
            cwd: None,
            additional_working_directories: Vec::new(),
            workflow_nesting_depth: 0,
            parent_session_id: parent_session_id.into(),
            agent_type_explicit: true,
            wait_for_completion: false,
            permission_broker: None,
            permission_prompts_unavailable: true,
        }
    }

    #[test]
    fn current_and_terminal_teammates_do_not_keep_the_default_team_alive() {
        let registry = TaskRegistry::new();
        let current_id = TaskId::new("current");
        register_in_process_teammate_task_with_cancel(
            &registry,
            InProcessTeammateTaskSpec {
                id: current_id.clone(),
                identity: TeammateIdentity {
                    agent_id: "current@default".into(),
                    agent_name: "current".into(),
                    team_name: "default".into(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "session-a".into(),
                },
                prompt: "work".into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                agent_type: None,
                description: None,
            },
            PromptCancel::new(),
        );

        assert!(!session_has_other_teammates(
            &registry,
            "session-a",
            &current_id
        ));

        let other_id = TaskId::new("other");
        register_in_process_teammate_task_with_cancel(
            &registry,
            InProcessTeammateTaskSpec {
                id: other_id.clone(),
                identity: TeammateIdentity {
                    agent_id: "other@default".into(),
                    agent_name: "other".into(),
                    team_name: "default".into(),
                    color: None,
                    plan_mode_required: false,
                    parent_session_id: "session-a".into(),
                },
                prompt: "other work".into(),
                model: None,
                model_profile: None,
                permission_mode: "default".into(),
                agent_type: None,
                description: None,
            },
            PromptCancel::new(),
        );

        assert!(session_has_other_teammates(
            &registry,
            "session-a",
            &current_id
        ));

        for terminal_status in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Killed,
        ] {
            registry.update(&other_id, |snapshot| snapshot.status = terminal_status);
            assert!(!session_has_other_teammates(
                &registry,
                "session-a",
                &current_id
            ));
        }
    }

    #[test]
    fn teammate_worker_filter_denies_recursive_team_tools() {
        let filter = teammate_worker_filter(false, None);

        assert!(!filter.is_unrestricted());
        assert!(!filter.allows("Agent", &[]));
        assert!(!filter.allows("TeamCreate", &[]));
        assert!(!filter.allows("TeamDelete", &[]));
        assert!(!filter.allows("SendMessage", &[]));
        assert!(!filter.allows("TaskStop", &[]));
        assert!(filter.allows("Read", &[]));
        assert!(filter.allows("Glob", &[]));
        assert!(filter.allows("Grep", &[]));
        assert!(filter.allows("Bash", &[]));
        assert!(filter.allows("Edit", &[]));
        assert!(filter.allows("TaskList", &[]));
        assert!(filter.allows("TaskGet", &[]));
        assert!(filter.allows("TaskUpdate", &[]));
    }

    #[test]
    fn teammate_worker_filter_plan_mode_denies_mutating_tools() {
        let filter = teammate_worker_filter(true, None);

        assert!(!filter.allows("Edit", &[]));
        assert!(!filter.allows("Write", &[]));
        assert!(!filter.allows("MultiEdit", &[]));
        assert!(!filter.allows("NotebookEdit", &[]));
        assert!(filter.allows("Read", &[]));
        assert!(filter.allows("Grep", &[]));
    }

    #[test]
    fn teammate_worker_filter_intersects_session_base_filter() {
        let base = SharedToolFilter::new(ToolFilter::unrestricted().with_deny(["Bash"]));
        let filter = teammate_worker_filter(false, Some(&base));

        assert!(!filter.allows("Bash", &[]));
        assert!(!filter.allows("Agent", &[]));
        assert!(filter.allows("Read", &[]));
    }

    #[tokio::test]
    async fn spawn_teammate_request_uses_explicit_worker_tool_filter() {
        let _home = TestHome::new("team-manager-filter");
        write_team_file(
            "filter-team",
            &TeamFile {
                name: "filter-team".into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: "team-lead@filter-team".into(),
                lead_session_id: None,
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();

        // What is under test is the *filter*: which of the tools a teammate
        // could have it actually gets. A bare engine no longer carries the
        // team and task tools — they moved to the `tasks` plugin, and `Agent`
        // to the `agents` plugin — so the test puts the real ones back on,
        // otherwise every negative assertion below passes vacuously.
        let mut engine = Engine::with_builtin_tools();
        for tool in rebon_plugin_tasks::tools()
            .into_iter()
            .chain(crate::tools())
        {
            engine.register_tool(tool);
        }
        let engine = Arc::new(engine);
        let mock_client = MockModelClient::new();
        mock_client.push_turn(text_turn("filter-1", "done"));
        let client: Arc<dyn ModelClient> = Arc::new(mock_client.clone());
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, client, registry.clone(), "mock");

        let result = manager
            .spawn_teammate(TeammateSpawnSpec {
                team_name: "filter-team".into(),
                name: "filter-agent".into(),
                prompt: "inspect filter".into(),
                agent_type: None,
                model: Some("mock".into()),
                model_profile: None,
                provider: None,
                mode: Some("default".into()),
                effort: None,
                description: None,
                system: Some("workflow shared worktree safety".into()),
                cwd: None,
                additional_working_directories: Vec::new(),
                workflow_nesting_depth: 1,
                parent_session_id: "session-filter".into(),
                agent_type_explicit: false,
                wait_for_completion: false,
                permission_broker: None,
                permission_prompts_unavailable: true,
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        let request = mock_client
            .captured_requests()
            .pop()
            .expect("worker request");
        let tool_names: std::collections::HashSet<&str> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();

        assert_eq!(
            request.system.as_deref(),
            Some("workflow shared worktree safety")
        );
        assert!(!tool_names.contains("Agent"));
        assert!(!tool_names.contains("TeamCreate"));
        assert!(!tool_names.contains("TeamDelete"));
        assert!(!tool_names.contains("SendMessage"));
        assert!(!tool_names.contains("TaskStop"));
        assert!(tool_names.contains("Read"));
        assert!(tool_names.contains("Glob"));
        assert!(tool_names.contains("Grep"));
        assert!(tool_names.contains("Bash"));
        assert!(tool_names.contains("Edit"));
        assert!(tool_names.contains("TaskList"));
        assert!(tool_names.contains("TaskGet"));
        assert!(tool_names.contains("TaskUpdate"));

        let snap = registry.snapshot(&TaskId::new(result.task_id)).unwrap();
        assert_eq!(snap.kind, TaskKind::InProcessTeammate);
    }

    #[test]
    fn teammate_permission_mode_defaults_and_preserves_plan() {
        let default_mode = normalize_teammate_permission_mode(None);
        assert_eq!(default_mode, "default");
        assert!(!teammate_plan_mode_required(&default_mode));

        let plan_mode = normalize_teammate_permission_mode(Some("plan"));
        assert_eq!(plan_mode, "plan");
        assert!(teammate_plan_mode_required(&plan_mode));
    }

    #[tokio::test]
    async fn spawn_teammate_registers_task_and_updates_idle_state() {
        let _home = TestHome::new("team-manager");
        write_team_file(
            "alpha",
            &TeamFile {
                name: "alpha".into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: "team-lead@alpha".into(),
                lead_session_id: None,
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-1", "done"));
        let client: Arc<dyn ModelClient> = Arc::new(client);
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, client, registry.clone(), "mock");

        let result = manager
            .spawn_teammate(TeammateSpawnSpec {
                team_name: "alpha".into(),
                name: "alice".into(),
                prompt: "do work".into(),
                agent_type: Some("general-purpose".into()),
                model: Some("mock".into()),
                model_profile: None,
                provider: None,
                mode: Some("default".into()),
                effort: None,
                description: None,
                system: None,
                cwd: None,
                additional_working_directories: Vec::new(),
                workflow_nesting_depth: 0,
                parent_session_id: "session-test".into(),
                agent_type_explicit: false,
                wait_for_completion: false,
                permission_broker: None,
                permission_prompts_unavailable: true,
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            manager.team_name_for_session("session-test").as_deref(),
            Some("alpha")
        );
        assert_eq!(
            read_team_file("alpha")
                .unwrap()
                .unwrap()
                .lead_session_id
                .as_deref(),
            Some("session-test")
        );
        let snap = registry.snapshot(&TaskId::new(result.task_id)).unwrap();
        assert_eq!(snap.kind, TaskKind::InProcessTeammate);
        if let TaskData::InProcessTeammate(data) = snap.data {
            assert!(data.is_idle);
            assert_eq!(data.identity.agent_name, "alice");
        } else {
            panic!("expected in-process teammate snapshot");
        }
    }

    #[tokio::test]
    async fn spawn_teammate_rejects_team_owned_by_another_session() {
        let _home = TestHome::new("team-manager-session-owner");
        write_team_file(
            "owned-team",
            &TeamFile {
                name: "owned-team".into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: "team-lead@owned-team".into(),
                lead_session_id: Some("session-a".into()),
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(
            Arc::new(Engine::with_builtin_tools()),
            Arc::new(MockModelClient::new()),
            registry.clone(),
            "mock",
        );

        let error = manager
            .spawn_teammate(test_spawn_spec(
                "owned-team",
                "intruder",
                "work",
                "session-b",
            ))
            .await
            .unwrap_err();

        assert!(error.contains("belongs to session `session-a`"), "{error}");
        assert!(registry.snapshots().is_empty());
        assert!(read_team_file("owned-team")
            .unwrap()
            .unwrap()
            .members
            .is_empty());
    }

    #[tokio::test]
    async fn session_default_teammate_does_not_auto_claim_task_board() {
        let _home = TestHome::new("session-default-no-auto-claim");
        let session_id = "session-default-no-auto-claim";
        let team_name = rebon_tool::ensure_session_default_team(session_id).unwrap();
        let task_id = rebon_tool::tasks::create_task(
            &team_name,
            rebon_tool::tasks::NewTask {
                subject: "leave this queued".into(),
                description: "only an explicit task worker should claim this".into(),
                ..rebon_tool::tasks::NewTask::default()
            },
        )
        .unwrap();

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-explicit", "explicit request done"));
        let captured_client = client.clone();
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, Arc::new(client), registry, "mock");
        let mut spec = test_spawn_spec(&team_name, "resident", "inspect only", session_id);
        spec.wait_for_completion = true;

        manager.spawn_teammate(spec).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(captured_client.call_count(), 1);
        let task = rebon_tool::tasks::list_tasks(&team_name)
            .unwrap()
            .into_iter()
            .find(|task| task.id == task_id)
            .unwrap();
        assert_eq!(task.status, TaskListStatus::Pending);
        assert!(task.owner.is_none());
    }

    #[tokio::test]
    async fn shutting_down_last_default_teammate_removes_team_data() {
        let _home = TestHome::new("session-default-last-shutdown");
        let session_id = "session-default-last-shutdown";
        let team_name = rebon_tool::ensure_session_default_team(session_id).unwrap();
        let team_dir = rebon_tool::team_files::team_dir(&team_name);

        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-shutdown", "done"));
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(
            Arc::new(Engine::with_builtin_tools()),
            Arc::new(client),
            registry,
            "mock",
        );
        let mut spec = test_spawn_spec(&team_name, "resident", "inspect", session_id);
        spec.wait_for_completion = true;
        manager.spawn_teammate(spec).await.unwrap();
        assert!(team_dir.is_dir());

        manager
            .request_shutdown(&team_name, "resident", None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while team_dir.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("last default teammate did not remove its session team");
    }

    #[tokio::test]
    async fn shutting_down_multiple_default_teammates_removes_team_after_the_last_exit() {
        let _home = TestHome::new("session-default-multiple-shutdown");
        let session_id = "session-default-multiple-shutdown";
        let team_name = rebon_tool::ensure_session_default_team(session_id).unwrap();
        let team_dir = rebon_tool::team_files::team_dir(&team_name);

        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-first", "first done"));
        client.push_turn(text_turn("msg-second", "second done"));
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(
            Arc::new(Engine::with_builtin_tools()),
            Arc::new(client),
            registry.clone(),
            "mock",
        );

        let mut first_spec = test_spawn_spec(&team_name, "first", "inspect first", session_id);
        first_spec.wait_for_completion = true;
        let first = manager.spawn_teammate(first_spec).await.unwrap();
        let mut second_spec = test_spawn_spec(&team_name, "second", "inspect second", session_id);
        second_spec.wait_for_completion = true;
        manager.spawn_teammate(second_spec).await.unwrap();

        manager
            .request_shutdown(&team_name, "first", None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let first_id = TaskId::new(first.task_id.clone());
            while registry
                .snapshot(&first_id)
                .is_some_and(|snapshot| !snapshot.status.is_terminal())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first default teammate did not shut down");
        assert!(team_dir.is_dir());

        manager
            .request_shutdown(&team_name, "second", None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while team_dir.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("last of multiple default teammates did not remove its session team");
    }

    #[tokio::test]
    async fn explicit_team_teammate_auto_claims_one_task_at_a_time() {
        let _home = TestHome::new("explicit-team-auto-claim");
        let team_name = "explicit-auto-claim-team";
        write_test_team(team_name);
        let task_id = rebon_tool::tasks::create_task(
            team_name,
            rebon_tool::tasks::NewTask {
                subject: "implement the queued change".into(),
                description: "keep the scope limited".into(),
                ..rebon_tool::tasks::NewTask::default()
            },
        )
        .unwrap();

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-explicit", "explicit request done"));
        client.push_turn(text_turn("msg-claimed", "claimed task done"));
        let captured_client = client.clone();
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, Arc::new(client), registry, "mock");
        let mut spec = test_spawn_spec(
            team_name,
            "task-worker",
            "finish the direct request",
            "session-explicit-auto-claim",
        );
        spec.wait_for_completion = true;

        manager.spawn_teammate(spec).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while captured_client.call_count() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("explicit team teammate did not claim the queued task");

        let requests = captured_client.captured_requests();
        let claimed_prompt = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                rebon_api::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(claimed_prompt.contains(&format!("Complete task #{task_id}:")));
        assert!(!claimed_prompt.contains("Complete all open tasks"));

        let task = rebon_tool::tasks::list_tasks(team_name)
            .unwrap()
            .into_iter()
            .find(|task| task.id == task_id)
            .unwrap();
        assert_eq!(task.status, TaskListStatus::InProgress);
        assert_eq!(task.owner.as_deref(), Some("task-worker"));
    }

    #[tokio::test]
    async fn named_teammate_redispatch_reuses_task_and_conversation() {
        let _home = TestHome::new("team-manager-redispatch");
        write_test_team("redispatch-team");

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-first", "remembered first result"));
        client.push_turn(text_turn("msg-second", "second result"));
        let captured_client = client.clone();
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, Arc::new(client), registry.clone(), "mock");

        let mut first_spec = test_spawn_spec(
            "redispatch-team",
            "researcher",
            "remember marker ALPHA",
            "session-redispatch",
        );
        first_spec.wait_for_completion = true;
        let first = manager.spawn_teammate(first_spec).await.unwrap();
        assert!(!first.reused);
        assert_eq!(
            first
                .handoff
                .as_ref()
                .map(|handoff| handoff.summary.as_str()),
            Some("remembered first result")
        );

        let mut second_spec = test_spawn_spec(
            "redispatch-team",
            "researcher",
            "use the earlier marker",
            "session-redispatch",
        );
        second_spec.agent_type = None;
        second_spec.agent_type_explicit = false;
        second_spec.wait_for_completion = true;
        let second = manager.spawn_teammate(second_spec).await.unwrap();

        assert!(second.reused);
        assert_eq!(second.task_id, first.task_id);
        assert_eq!(second.agent_id, first.agent_id);
        assert_eq!(
            second
                .handoff
                .as_ref()
                .map(|handoff| handoff.summary.as_str()),
            Some("second result")
        );
        assert_eq!(registry.snapshots().len(), 1);

        let requests = captured_client.captured_requests();
        assert_eq!(requests.len(), 2);
        let second_request_text = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                rebon_api::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(second_request_text.contains("remember marker ALPHA"));
        assert!(second_request_text.contains("remembered first result"));
        assert!(second_request_text.contains("use the earlier marker"));

        let snapshot = registry.snapshot(&TaskId::new(first.task_id)).unwrap();
        let TaskData::InProcessTeammate(data) = snapshot.data else {
            panic!("expected teammate");
        };
        assert_eq!(snapshot.status, TaskStatus::Running);
        assert!(data.is_idle);

        let roster = manager.teammate_roster("session-redispatch");
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].name, "researcher");
        assert_eq!(roster[0].agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(roster[0].status, "idle");
        assert_eq!(roster[0].last_result.as_deref(), Some("second result"));
        assert!(roster[0].current_task.is_none());
    }

    #[tokio::test]
    async fn closing_owner_session_removes_default_team_and_teammate() {
        let _home = TestHome::new("team-manager-session-close");
        let session_id = "session-close";
        let team_name = rebon_tool::ensure_session_default_team(session_id).unwrap();
        let team_dir = rebon_tool::team_files::team_dir(&team_name);

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-close", "done"));
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, Arc::new(client), registry.clone(), "mock");
        let mut spec = test_spawn_spec(&team_name, "closable", "work", session_id);
        spec.wait_for_completion = true;
        manager.spawn_teammate(spec).await.unwrap();

        assert!(team_dir.is_dir());
        manager.close_session(session_id);
        assert!(registry.snapshots().is_empty());
        assert!(!team_dir.exists());
        tokio::task::yield_now().await;
    }

    #[test]
    fn closing_owner_session_clears_explicit_team_binding_without_deleting_team() {
        let _home = TestHome::new("team-manager-explicit-session-close");
        let session_id = "session-explicit-close";
        write_team_file(
            "explicit-close-team",
            &TeamFile {
                name: "explicit-close-team".into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: "team-lead@explicit-close-team".into(),
                lead_session_id: Some(session_id.into()),
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();
        let manager = InProcessTeamManager::new(
            Arc::new(Engine::with_builtin_tools()),
            Arc::new(MockModelClient::new()),
            TaskRegistry::new(),
            "mock",
        );

        manager.close_session(session_id);

        let team = read_team_file("explicit-close-team").unwrap().unwrap();
        assert_eq!(team.lead_session_id, None);
        assert_eq!(rebon_tool::team_name_for_session(session_id).unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_session_close_prevents_late_teammate_registration() {
        let _home = TestHome::new("team-manager-concurrent-close");
        let session_id = "session-concurrent-close";
        let team_name = rebon_tool::ensure_session_default_team(session_id).unwrap();
        let team_dir = rebon_tool::team_files::team_dir(&team_name);

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-concurrent-close", "done"));
        let registry = TaskRegistry::new();
        let manager = Arc::new(InProcessTeamManager::new(
            engine,
            Arc::new(client),
            registry.clone(),
            "mock",
        ));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let close_manager = manager.clone();
        let close_barrier = barrier.clone();
        let close_session_id = session_id.to_string();
        let close = tokio::task::spawn_blocking(move || {
            close_barrier.wait();
            close_manager.close_session(&close_session_id);
        });
        let spawn_manager = manager.clone();
        let spawn_barrier = barrier.clone();
        let spawn_spec = test_spawn_spec(&team_name, "racy", "work", session_id);
        let spawn = tokio::spawn(async move {
            spawn_barrier.wait();
            spawn_manager.spawn_teammate(spawn_spec).await
        });

        barrier.wait();
        close.await.unwrap();
        let spawn_result = spawn.await.unwrap();
        if let Err(error) = spawn_result {
            assert!(error.contains("session `session-concurrent-close` is closed"));
        }
        tokio::task::yield_now().await;

        assert!(registry.snapshots().is_empty());
        assert!(!team_dir.exists());
        assert!(manager
            .recipes
            .lock()
            .expect("teammate recipe registry poisoned")
            .is_empty());
    }

    #[tokio::test]
    async fn dropping_last_manager_closes_its_owner_sessions() {
        let _home = TestHome::new("team-manager-drop-close");
        let session_id = "session-drop-close";
        let team_name = rebon_tool::ensure_session_default_team(session_id).unwrap();
        let team_dir = rebon_tool::team_files::team_dir(&team_name);

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-drop-close", "done"));
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, Arc::new(client), registry.clone(), "mock");
        let keep_alive = manager.clone();
        let mut spec = test_spawn_spec(&team_name, "drop-closable", "work", session_id);
        spec.wait_for_completion = true;
        manager.spawn_teammate(spec).await.unwrap();

        drop(manager);
        assert_eq!(registry.snapshots().len(), 1);
        drop(keep_alive);
        assert!(registry.snapshots().is_empty());
        assert!(!team_dir.exists());
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn named_teammate_rejects_explicit_agent_type_change() {
        let _home = TestHome::new("team-manager-type-conflict");
        write_test_team("type-team");

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-first", "done"));
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, Arc::new(client), registry, "mock");

        let mut first = test_spawn_spec("type-team", "same-name", "first", "session-type");
        first.wait_for_completion = true;
        manager.spawn_teammate(first).await.unwrap();

        let mut conflict = test_spawn_spec("type-team", "same-name", "follow up", "session-type");
        conflict.agent_type = Some("Explore".into());
        conflict.agent_type_explicit = true;
        let error = manager.spawn_teammate(conflict).await.unwrap_err();

        assert!(error.contains("already has agent type `general-purpose`"));
        assert!(error.contains("cannot redispatch it as `Explore`"));
    }

    struct FailOnceModelRouter {
        attempts: AtomicUsize,
        client: Arc<dyn ModelClient>,
    }

    #[async_trait]
    impl AgentModelRouter for FailOnceModelRouter {
        async fn resolve(
            &self,
            _request: ModelRouteRequest,
        ) -> anyhow::Result<rebon_agent_core::model_router::ResolvedModelRuntime> {
            if self.attempts.fetch_add(1, AtomicOrdering::Relaxed) == 0 {
                anyhow::bail!("transient router failure");
            }
            Ok(rebon_agent_core::model_router::ResolvedModelRuntime {
                provider_name: "mock".into(),
                client: self.client.clone(),
                model: "mock".into(),
                reasoning_effort: None,
            })
        }
    }

    #[tokio::test]
    async fn failed_named_teammate_is_revived_in_place_on_redispatch() {
        let _home = TestHome::new("team-manager-revive");
        write_test_team("revive-team");

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-revived", "recovered result"));
        let routed_client: Arc<dyn ModelClient> = Arc::new(client.clone());
        let router = Arc::new(FailOnceModelRouter {
            attempts: AtomicUsize::new(0),
            client: routed_client.clone(),
        });
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, routed_client, registry.clone(), "mock")
            .with_model_router(router);

        let mut first = test_spawn_spec(
            "revive-team",
            "resilient",
            "first request",
            "session-revive",
        );
        first.wait_for_completion = true;
        let failed = manager.spawn_teammate(first).await.unwrap();
        assert!(!failed.reused);
        assert_eq!(
            failed
                .handoff
                .as_ref()
                .map(|handoff| handoff.status.as_str()),
            Some("failed")
        );
        assert_eq!(
            registry
                .snapshot(&TaskId::new(&failed.task_id))
                .unwrap()
                .status,
            TaskStatus::Failed
        );

        let mut retry = test_spawn_spec(
            "revive-team",
            "resilient",
            "retry the request",
            "session-revive",
        );
        retry.wait_for_completion = true;
        let revived = manager.spawn_teammate(retry).await.unwrap();

        assert!(revived.reused);
        assert_eq!(revived.task_id, failed.task_id);
        assert_eq!(
            revived
                .handoff
                .as_ref()
                .map(|handoff| handoff.summary.as_str()),
            Some("recovered result")
        );
        assert_eq!(registry.snapshots().len(), 1);
        let snapshot = registry.snapshot(&TaskId::new(revived.task_id)).unwrap();
        let TaskData::InProcessTeammate(data) = snapshot.data else {
            panic!("expected teammate");
        };
        assert_eq!(snapshot.status, TaskStatus::Running);
        assert!(data.is_idle);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn failed_named_teammate_is_revived_in_place_by_task_message() {
        let _home = TestHome::new("team-manager-task-message-revive");
        write_test_team("task-message-revive-team");

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn(
            "msg-task-revived",
            "recovered through task reply",
        ));
        let routed_client: Arc<dyn ModelClient> = Arc::new(client.clone());
        let router = Arc::new(FailOnceModelRouter {
            attempts: AtomicUsize::new(0),
            client: routed_client.clone(),
        });
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, routed_client, registry.clone(), "mock")
            .with_model_router(router);

        let mut first = test_spawn_spec(
            "task-message-revive-team",
            "resilient",
            "first request",
            "session-task-message-revive",
        );
        first.wait_for_completion = true;
        let failed = manager.spawn_teammate(first).await.unwrap();
        assert_eq!(
            registry
                .snapshot(&TaskId::new(&failed.task_id))
                .unwrap()
                .status,
            TaskStatus::Failed
        );

        manager
            .send_message_to_task(&failed.task_id, "retry from task UI".into())
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = registry.snapshot(&TaskId::new(&failed.task_id)).unwrap();
                if snapshot.status == TaskStatus::Running
                    && matches!(&snapshot.data, TaskData::InProcessTeammate(data) if data.is_idle)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("revived teammate did not finish its task reply");

        assert_eq!(registry.snapshots().len(), 1);
        assert_eq!(client.call_count(), 1);
        let roster = manager.teammate_roster("session-task-message-revive");
        assert_eq!(roster.len(), 1);
        assert_eq!(
            roster[0].last_result.as_deref(),
            Some("recovered through task reply")
        );
    }

    #[tokio::test]
    async fn concurrent_same_name_dispatch_creates_one_teammate() {
        let _home = TestHome::new("team-manager-concurrent-dispatch");
        write_test_team("concurrent-team");

        let engine = Arc::new(Engine::with_builtin_tools());
        let client = MockModelClient::new();
        client.push_turn(text_turn("msg-one", "one"));
        client.push_turn(text_turn("msg-two", "two"));
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, Arc::new(client), registry.clone(), "mock");
        let first_spec = test_spawn_spec(
            "concurrent-team",
            "singleton",
            "first request",
            "session-concurrent",
        );
        let mut second_spec = first_spec.clone();
        second_spec.prompt = "second request".into();

        let (first, second) = tokio::join!(
            manager.spawn_teammate(first_spec),
            manager.spawn_teammate(second_spec)
        );
        let first = first.unwrap();
        let second = second.unwrap();

        assert_eq!(first.task_id, second.task_id);
        assert_ne!(first.reused, second.reused);
        assert_eq!(registry.snapshots().len(), 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snapshot = registry.snapshot(&TaskId::new(first.task_id)).unwrap();
        let TaskData::InProcessTeammate(data) = snapshot.data else {
            panic!("expected teammate");
        };
        assert!(data.is_idle);
        assert!(data.pending_user_messages.is_empty());
    }

    #[tokio::test]
    async fn spawn_teammate_into_missing_team_errors_actionably_without_creating_dir() {
        let _home = TestHome::new("team-manager-missing");
        let engine = Arc::new(Engine::with_builtin_tools());
        let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, client, registry.clone(), "mock");

        let err = manager
            .spawn_teammate(TeammateSpawnSpec {
                team_name: "ghost-team".into(),
                name: "alice".into(),
                prompt: "do work".into(),
                agent_type: None,
                model: None,
                model_profile: None,
                provider: None,
                mode: None,
                effort: None,
                description: None,
                system: None,
                cwd: None,
                additional_working_directories: Vec::new(),
                workflow_nesting_depth: 0,
                parent_session_id: "session-test".into(),
                agent_type_explicit: false,
                wait_for_completion: false,
                permission_broker: None,
                permission_prompts_unavailable: false,
            })
            .await
            .unwrap_err();

        assert!(err.contains("team `ghost-team` does not exist"), "{err}");
        assert!(err.contains("TeamCreate"), "{err}");
        assert!(
            !rebon_tool::team_files::team_dir("ghost-team").exists(),
            "failed spawn must not leave an empty team dir behind"
        );
        assert!(registry.snapshots().is_empty());
    }

    // ── fork_for_sub_agent ──────────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Client wrapper that tracks `fork_for_sub_agent` calls and
    /// returns a child MockModelClient with its own scripted turn.
    struct ForkTrackingClient {
        parent_mock: Arc<MockModelClient>,
        child_mock: Arc<MockModelClient>,
        fork_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelClient for ForkTrackingClient {
        fn provider_name(&self) -> &'static str {
            "fork-tracking"
        }

        async fn create_message_stream(
            &self,
            request: rebon_api::CreateMessageRequest,
        ) -> rebon_api::ModelResult<rebon_api::StreamEventStream> {
            self.parent_mock.create_message_stream(request).await
        }

        fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
            self.fork_count.fetch_add(1, AtomicOrdering::Relaxed);
            Some(self.child_mock.clone() as Arc<dyn ModelClient>)
        }
    }

    #[tokio::test]
    async fn spawn_teammate_uses_forked_client_when_available() {
        let _home = TestHome::new("team-manager-fork");
        write_team_file(
            "beta",
            &TeamFile {
                name: "beta".into(),
                description: None,
                created_at: now_wall_ms(),
                lead_agent_id: "team-lead@beta".into(),
                lead_session_id: None,
                hidden_pane_ids: Vec::new(),
                members: Vec::new(),
            },
        )
        .unwrap();

        let engine = Arc::new(Engine::with_builtin_tools());
        let parent_mock = Arc::new(MockModelClient::new());
        // Parent mock has NO scripted turns — if the teammate
        // accidentally used it, the test would fail.

        let child_mock = Arc::new(MockModelClient::new());
        child_mock.push_turn(text_turn("fork-1", "forked teammate done"));

        let fork_count = Arc::new(AtomicUsize::new(0));
        let client: Arc<dyn ModelClient> = Arc::new(ForkTrackingClient {
            parent_mock: parent_mock.clone(),
            child_mock: child_mock.clone(),
            fork_count: fork_count.clone(),
        });

        let registry = TaskRegistry::new();
        let manager = InProcessTeamManager::new(engine, client, registry.clone(), "mock");

        let result = manager
            .spawn_teammate(TeammateSpawnSpec {
                team_name: "beta".into(),
                name: "bob".into(),
                prompt: "task for bob".into(),
                agent_type: None,
                model: Some("mock".into()),
                model_profile: None,
                provider: None,
                mode: Some("default".into()),
                effort: None,
                description: None,
                system: None,
                cwd: None,
                additional_working_directories: Vec::new(),
                workflow_nesting_depth: 0,
                parent_session_id: "session-test".into(),
                agent_type_explicit: false,
                wait_for_completion: false,
                permission_broker: None,
                permission_prompts_unavailable: true,
            })
            .await
            .unwrap();

        // Wait for the async teammate loop to complete its first turn.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // fork_for_sub_agent was called.
        assert_eq!(fork_count.load(AtomicOrdering::Relaxed), 1);
        // Child mock was used (parent mock was never called).
        assert_eq!(child_mock.call_count(), 1);
        assert_eq!(parent_mock.call_count(), 0);

        let snap = registry.snapshot(&TaskId::new(result.task_id)).unwrap();
        assert_eq!(snap.kind, TaskKind::InProcessTeammate);
    }
}
