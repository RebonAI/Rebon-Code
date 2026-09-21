//! The resident backend pool behind externally-routed sub-agents.
//!
//! One [`AcpAgentBackend`] per declared agent, built lazily and kept as
//! a reconnectable handle. Concurrent tasks for the same agent share
//! its live process, but the last task to finish shuts that process
//! down immediately; a later task reconnects through the same backend.
//! Every task gets a **fresh agent session**
//! (`start_session_with_meta` under a unique host session id, whose
//! routing is forgotten on the way out), so tasks never see each
//! other's context. The
//! `Created`-always shape is also what makes the model hint reliable:
//! sessionMeta is only sent on `session/new`/`session/load`, never on a
//! `Reused` start.
//!
//! Deliberately **not** the per-session `SessionAgents` registry: that
//! one binds journal, file history and handoff to a single Rebon
//! session (`journal.rs::check_session`, `SessionFileHistory::check`).
//! The pool serves tasks from any turn of the owning session, so it
//! carries a no-op journal (the task's output returns through
//! `SubAgentResult`, not the transcript), no handoff provider (the
//! brief block plays that role), and a file history that forwards to
//! the session's tracker without a session-id gate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rebon_acp_client::{
    AcpAgentBackend, AcpBackendConfig, AgentCommand, HostFileHistory, SnapshotHostFs,
};
use rebon_agent_core::backend::AgentSessionSpec;
use rebon_agent_core::prompt_executor::{PromptExecutorError, PromptRequest};
use rebon_agent_core::publisher::{
    make_permission_result_response, ChannelPermissionRequestPublisher, SessionUpdatePublisher,
};
use rebon_agent_core::AgentBackend;
use rebon_proto::types::{
    PermissionOptionKind, PermissionOutcome, RequestPermissionResult, SessionUpdate,
    SessionUpdateParams, StopReason,
};
use rebon_tool::external_agent::{
    ExternalSubAgentRunner, ExternalTaskEvent, ExternalTaskOutcome, ExternalTaskRequest,
    ExternalTaskStatus,
};

use rebon_agent_core::routing::DeclaredAgent;

/// File history for pool tasks: the session's tracker, no session-id
/// gate. The tracker itself is single-session (it belongs to the
/// owning Rebon session), so routed writes land in the same store the
/// local `Write` tool uses and `/rewind` covers them; the gate the
/// per-session adapter adds would reject the pool's synthetic
/// `subagent-*` session ids.
struct PoolFileHistory {
    tracker: Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
}

impl HostFileHistory for PoolFileHistory {
    fn begin_turn(&self, _session_id: &str, turn_id: &str) -> anyhow::Result<()> {
        self.tracker.begin_prompt_turn(turn_id)
    }

    fn snapshot_before_write(&self, _session_id: &str, path: &Path) -> anyhow::Result<()> {
        self.tracker.track_before_write(path)
    }

    fn end_turn(&self, _session_id: &str, turn_id: &str) -> anyhow::Result<()> {
        self.tracker.end_prompt_turn(turn_id)
    }

    fn is_armed(&self) -> Option<bool> {
        self.tracker.is_armed()
    }
}

struct PoolEntry {
    declared: DeclaredAgent,
    /// Built once, synchronously — constructing a backend spawns
    /// nothing (connecting is lazy inside it), so a sync cell is
    /// enough and keeps `resolve_agent`/`backend_for` non-async.
    backend: std::sync::OnceLock<Arc<AcpAgentBackend>>,
    /// Serializes task admission with last-task shutdown. Holding this
    /// lock while shutting down prevents a new task from reconnecting
    /// just before the old task kills the shared process.
    active_tasks: tokio::sync::Mutex<usize>,
}

/// One reconnectable [`AcpAgentBackend`] per declared agent.
pub struct AcpSubAgentPool {
    entries: HashMap<String, Arc<PoolEntry>>,
    cwd: String,
    write_roots: Vec<PathBuf>,
    tracker: Option<Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>>,
    /// The host half of the injected fs tools, started with the first
    /// backend that wants them and held so its port stays bound as
    /// long as the pool. `None` inside means the start was attempted
    /// and declined (no tracker, no runtime, or the bind failed) —
    /// those tasks run without the injected tools, same degradation as
    /// the session-level registry.
    fs_service: std::sync::OnceLock<Option<rebon_acp_client::HostFsServiceHandle>>,
}

/// The auth-domain label the pool's fs service and its bridges agree
/// on. Pool tasks use synthetic per-task session ids, but the service
/// is pool-wide and [`PoolFileHistory`] ignores the session id anyway;
/// the label only has to match between `start` and `bridge_env`.
const POOL_FS_SESSION_LABEL: &str = "subagent-pool";

fn fold_id(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

impl AcpSubAgentPool {
    /// `tracker` is the owning session's file-history tracker; `None`
    /// (headless holders without file history) downgrades routed
    /// writes to direct disk access.
    pub fn new(
        declared: Vec<DeclaredAgent>,
        cwd: String,
        write_roots: Vec<PathBuf>,
        tracker: Option<Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>>,
    ) -> Arc<Self> {
        let mut entries = HashMap::new();
        for agent in declared {
            let folded = fold_id(&agent.id);
            if entries.contains_key(&folded) {
                continue;
            }
            entries.insert(
                folded,
                Arc::new(PoolEntry {
                    declared: agent,
                    backend: std::sync::OnceLock::new(),
                    active_tasks: tokio::sync::Mutex::new(0),
                }),
            );
        }
        Arc::new(Self {
            entries,
            cwd,
            write_roots,
            tracker,
            fs_service: std::sync::OnceLock::new(),
        })
    }

    /// The injected `rebon-fs` MCP server entry for this pool's
    /// backends, starting the pool-wide host service on first use.
    /// `None` degrades to no injection: the agent's writes then reach
    /// disk only through ACP `fs/write_text_file` (if the agent uses
    /// it), not through Rebon's injected tools.
    fn injected_fs_server(&self) -> Option<rebon_proto::types::McpServerConfig> {
        let service = self
            .fs_service
            .get_or_init(|| {
                let tracker = self.tracker.as_ref()?;
                let file_history: Arc<dyn HostFileHistory> = Arc::new(PoolFileHistory {
                    tracker: tracker.clone(),
                });
                rebon_harness::agent_assembly::start_fs_service(
                    file_history,
                    &self.write_roots,
                    POOL_FS_SESSION_LABEL,
                )
            })
            .as_ref()?;
        rebon_harness::agent_assembly::injected_fs_server(service, POOL_FS_SESSION_LABEL)
    }

    fn backend_for(&self, folded: &str) -> Option<Arc<AcpAgentBackend>> {
        let entry = self.entries.get(folded)?;
        let backend = entry.backend.get_or_init(|| {
            let declared = &entry.declared;
            let mut command = AgentCommand::new(&declared.command)
                .with_args(declared.args.clone())
                .with_cwd(declared.cwd.clone().unwrap_or_else(|| self.cwd.clone()))
                .with_spawn_hint(declared.install_hint.clone());
            for (key, value) in &declared.env {
                command = command.with_env(key, value);
            }
            // `AcpBackendConfig::new` already defaults to the no-op
            // journal and direct host fs; only the snapshot pipeline
            // is opted into below.
            let mut config = AcpBackendConfig::new(&declared.id, command);
            if let Some(tracker) = &self.tracker {
                config = config.with_host_fs(Arc::new(SnapshotHostFs::new(
                    Arc::new(PoolFileHistory {
                        tracker: tracker.clone(),
                    }),
                    self.write_roots.iter().cloned(),
                )));
            }
            // Injected write_file/edit_file are what actually route the
            // agent's writes through the snapshot pipeline — the host_fs
            // above only catches agents that write via ACP fs calls.
            if declared.inject_fs_tools {
                if let Some(injected) = self.injected_fs_server() {
                    config = config.with_injected_mcp_servers(vec![injected]);
                }
            }
            if declared.session_meta.is_some() {
                config = config.with_session_meta(declared.session_meta.clone());
            }
            Arc::new(AcpAgentBackend::new(config))
        });
        Some(backend.clone())
    }

    async fn acquire_task(&self, folded: &str) -> Option<ActiveTaskLease> {
        let entry = self.entries.get(folded)?.clone();
        let backend = self.backend_for(folded)?;
        let mut active_tasks = entry.active_tasks.lock().await;
        *active_tasks += 1;
        drop(active_tasks);
        Some(ActiveTaskLease {
            entry,
            backend,
            released: false,
        })
    }

    /// Stop every backend this pool ever started. Call at the owning
    /// session's shutdown point; `kill_on_drop` remains the backstop.
    pub async fn shutdown(&self) {
        for entry in self.entries.values() {
            let _active_tasks = entry.active_tasks.lock().await;
            if let Some(backend) = entry.backend.get() {
                backend.shutdown().await;
            }
        }
    }

    /// Pre-fill an agent's backend slot, so tests can substitute an
    /// in-process fake agent for the real process spawn.
    #[cfg(test)]
    fn inject_backend_for_test(&self, agent_id: &str, backend: Arc<AcpAgentBackend>) {
        let entry = self
            .entries
            .get(&fold_id(agent_id))
            .expect("declared agent");
        entry
            .backend
            .set(backend)
            .unwrap_or_else(|_| panic!("backend already built for {agent_id}"));
    }
}

impl PoolEntry {
    async fn release_task(&self, backend: Arc<AcpAgentBackend>) {
        let mut active_tasks = self.active_tasks.lock().await;
        if *active_tasks == 0 {
            tracing::warn!(
                agent = %self.declared.id,
                "acp-subagent: task lease released after the entry was already idle"
            );
            return;
        }
        *active_tasks -= 1;
        if *active_tasks == 0 {
            backend.shutdown().await;
        }
    }
}

struct ActiveTaskLease {
    entry: Arc<PoolEntry>,
    backend: Arc<AcpAgentBackend>,
    released: bool,
}

impl ActiveTaskLease {
    fn backend(&self) -> Arc<AcpAgentBackend> {
        self.backend.clone()
    }

    async fn finish(mut self) {
        self.released = true;
        let entry = self.entry.clone();
        let backend = self.backend.clone();
        let cleanup = tokio::spawn(async move {
            entry.release_task(backend).await;
        });
        if let Err(err) = cleanup.await {
            tracing::warn!(error = %err, "acp-subagent: idle cleanup task failed");
        }
    }
}

impl Drop for ActiveTaskLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let entry = self.entry.clone();
        let backend = self.backend.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    entry.release_task(backend).await;
                });
            }
            Err(err) => {
                tracing::warn!(
                    agent = %entry.declared.id,
                    error = %err,
                    "acp-subagent: could not schedule idle cleanup"
                );
            }
        }
    }
}

/// Session `_meta` overlay carrying the model hint: the adapter-specific
/// spelling for claude-agent-acp plus a generic key for adapters that
/// grow support later. Merged over the declared `sessionMeta`, so a
/// user-configured recipe (disallowedTools etc.) survives.
fn model_hint_overlay(hint: &str) -> HashMap<String, serde_json::Value> {
    HashMap::from([
        (
            "claudeCode".to_string(),
            serde_json::json!({ "options": { "model": hint } }),
        ),
        ("modelHint".to_string(), serde_json::json!(hint)),
    ])
}

/// Accumulates the agent's update stream into the task's outcome:
/// answer text, tool-call count, and live events for the sink.
struct TaskUpdateCollector {
    final_text: Mutex<String>,
    tool_calls: Mutex<usize>,
    tool_titles: Mutex<HashMap<String, String>>,
    sink: Option<Arc<dyn Fn(ExternalTaskEvent) + Send + Sync>>,
}

#[async_trait]
impl SessionUpdatePublisher for TaskUpdateCollector {
    async fn publish_owned(&self, params: SessionUpdateParams) {
        match params.update {
            SessionUpdate::AgentMessageChunk { content } => {
                if let rebon_types::ContentBlock::Text(text) = content {
                    let delta = text.text;
                    self.final_text.lock().unwrap().push_str(&delta);
                    if let Some(sink) = &self.sink {
                        sink(ExternalTaskEvent::AgentText(delta));
                    }
                }
            }
            SessionUpdate::ThinkingDelta { text } => {
                if let Some(sink) = &self.sink {
                    sink(ExternalTaskEvent::Thinking(text));
                }
            }
            SessionUpdate::ThinkingEnd => {
                if let Some(sink) = &self.sink {
                    sink(ExternalTaskEvent::ThinkingEnd);
                }
            }
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                raw_input,
                ..
            } => {
                *self.tool_calls.lock().unwrap() += 1;
                self.tool_titles
                    .lock()
                    .unwrap()
                    .insert(tool_call_id.clone(), title.clone());
                if let Some(sink) = &self.sink {
                    sink(ExternalTaskEvent::ToolCall {
                        tool_call_id,
                        title,
                        input: raw_input
                            .map(|input| serde_json::Value::Object(input.into_iter().collect())),
                    });
                }
            }
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                title,
                status,
                raw_output,
                ..
            } => {
                let title = {
                    let mut titles = self.tool_titles.lock().unwrap();
                    if let Some(title) = title {
                        titles.insert(tool_call_id.clone(), title.clone());
                        title
                    } else {
                        titles
                            .get(&tool_call_id)
                            .cloned()
                            .unwrap_or_else(|| tool_call_id.clone())
                    }
                };
                if let Some(sink) = &self.sink {
                    sink(ExternalTaskEvent::ToolCallUpdate {
                        tool_call_id,
                        title,
                        status,
                        output: raw_output
                            .map(|output| serde_json::Value::Object(output.into_iter().collect())),
                    });
                }
            }
            _ => {}
        }
    }
}

/// Answer the agent's permission requests without a user in the loop.
///
/// Policy: `AllowOnce` when offered, and never `AllowAlways`, because
/// a standing grant must come from a human. Anything else means reject. This mirrors the
/// local sub-agent broker (auto-approve except sensitive) in effect:
/// an auto-deny would leave the external agent unable to complete any
/// write task, failing in ways that look like the agent's fault. The
/// blast radius stays bounded by the agent's own permission mode,
/// which the user can tighten via `sessionMeta`.
fn spawn_auto_permission_responder(
    agent_id: String,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<
        rebon_agent_core::publisher::OutboundPermissionRequest,
    >,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(outbound) = receiver.recv().await {
            let options = &outbound.params.options;
            let chosen = options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::AllowOnce);
            let (outcome, option_id, decision) = match chosen {
                Some(option) => (
                    PermissionOutcome::Selected,
                    Some(option.option_id.clone()),
                    "allow-once",
                ),
                None => (PermissionOutcome::Cancelled, None, "reject"),
            };
            tracing::info!(
                agent = %agent_id,
                tool = outbound.params.tool_name.as_deref().unwrap_or("?"),
                decision,
                "acp-subagent: auto permission decision"
            );
            let _ = outbound.response_tx.send(make_permission_result_response(
                outbound.request_id,
                RequestPermissionResult {
                    outcome,
                    option_id,
                    updated_input: None,
                },
            ));
        }
    })
}

/// Releases the task's session routing on every exit path. ACP has no
/// verb for closing a session on the agent's side, so the host forgets
/// the mapping immediately and the last active task then shuts down the
/// process, taking the agent-side session context with it.
struct SessionCloseGuard {
    backend: Arc<AcpAgentBackend>,
    session_id: String,
}

impl Drop for SessionCloseGuard {
    fn drop(&mut self) {
        self.backend.forget_session(&self.session_id);
    }
}

#[async_trait]
impl ExternalSubAgentRunner for AcpSubAgentPool {
    fn resolve_agent(&self, prefix: &str) -> Option<String> {
        self.entries
            .get(&fold_id(prefix))
            .map(|entry| entry.declared.id.clone())
    }

    async fn run_task(&self, request: ExternalTaskRequest) -> Result<ExternalTaskOutcome, String> {
        let folded = fold_id(&request.agent_id);
        let lease = self
            .acquire_task(&folded)
            .await
            .ok_or_else(|| format!("agent `{}` is not in the sub-agent pool", request.agent_id))?;
        let backend = lease.backend();

        let result = async {
            // Fresh session per task: unique host session id → Created →
            // the meta overlay (model hint) definitely goes on the wire.
            let cwd = request.cwd.clone().unwrap_or_else(|| self.cwd.clone());
            let spec = AgentSessionSpec::new(&request.task_session_id, &cwd);
            let overlay = request.model_hint.as_deref().map(model_hint_overlay);
            let start = backend
                .start_session_with_meta(spec, overlay)
                .await
                .map_err(|err| format!("could not start a session: {err}"))?;
            let _close = SessionCloseGuard {
                backend: backend.clone(),
                session_id: request.task_session_id.clone(),
            };

            let collector = Arc::new(TaskUpdateCollector {
                final_text: Mutex::new(String::new()),
                tool_calls: Mutex::new(0),
                tool_titles: Mutex::new(HashMap::new()),
                sink: request.progress.clone(),
            });
            let (permission_publisher, permission_rx) = ChannelPermissionRequestPublisher::new();
            let responder =
                spawn_auto_permission_responder(request.agent_id.clone(), permission_rx);

            let prompt_request = PromptRequest {
                user_prompt: None,
                effort_is_session_default: false,
                session_id: request.task_session_id.clone(),
                cwd,
                prompt: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: request.prompt.clone(),
                    annotations: None,
                })],
                mcp_servers: Vec::new(),
                update_publisher: Some(collector.clone() as Arc<dyn SessionUpdatePublisher>),
                permission_publisher: Some(permission_publisher),
                cancel: request.cancel.clone(),
                thinking_budget: None,
                max_tokens: None,
                reasoning_effort_ordinal: None,
                additional_working_directories: Vec::new(),
                coordinator_mode: None,
                coordinator_report_paths: Vec::new(),
                user_message_uuid: None,
                background_agent_system: None,
                background_agent_tool_filter: None,
                execution_policy: None,
                replay_requests: Vec::new(),
                skill_invocations: Vec::new(),
            };
            let outcome = AgentBackend::prompt(backend.as_ref(), prompt_request).await;
            responder.abort();

            let final_text = collector.final_text.lock().unwrap().clone();
            let tool_call_count = *collector.tool_calls.lock().unwrap();
            let status = match outcome {
                Ok(outcome) => match outcome.stop_reason {
                    StopReason::Cancelled => ExternalTaskStatus::Cancelled,
                    _ => ExternalTaskStatus::Completed,
                },
                Err(PromptExecutorError::Cancelled) => ExternalTaskStatus::Cancelled,
                Err(err) => ExternalTaskStatus::Failed(err.to_string()),
            };
            Ok(ExternalTaskOutcome {
                final_text,
                status,
                tool_call_count,
                acp_session_id: Some(start.session_id),
            })
        }
        .await;
        lease.finish().await;
        result
    }
}

/// Build the pool's declared-agent list exactly the way the session
/// surface does, so `/agent list` and the sub-agent model axis can
/// never disagree about what an id means.
pub fn pool_from_declared(
    declared: Vec<DeclaredAgent>,
    cwd: String,
    additional_write_roots: Vec<PathBuf>,
    tracker: Option<Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>>,
) -> Arc<AcpSubAgentPool> {
    let mut write_roots = vec![PathBuf::from(&cwd)];
    write_roots.extend(additional_write_roots);
    AcpSubAgentPool::new(declared, cwd, write_roots, tracker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rebon_acp_client::{
        default_client_capabilities, AcpClient, AgentConnector, ClientDelegate, ClientError,
        ConnectionError,
    };
    use rebon_proto::types::{PermissionOption, RequestPermissionParams, ToolCallReference};
    use rebon_types::PromptCancel;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    use rebon_agent_core::routing::AgentOrigin;

    fn declared(id: &str) -> DeclaredAgent {
        DeclaredAgent {
            id: id.to_string(),
            label: id.to_string(),
            command: "unused-in-tests".to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            origin: AgentOrigin::Config,
            inject_fs_tools: false,
            session_meta: Some(std::collections::HashMap::from([(
                "claudeCode".to_string(),
                serde_json::json!({"options": {"disallowedTools": ["Write"]}}),
            )])),
            install_hint: None,
            workspace_cwd: None,
        }
    }

    /// A minimal in-process ACP agent: answers initialize/session-new,
    /// streams thinking, text, and a complete tool lifecycle per prompt,
    /// then ends the turn. Records every `session/new`'s id and `_meta`.
    async fn serve_fake_agent(
        mut stream: DuplexStream,
        sessions: Arc<Mutex<Vec<serde_json::Value>>>,
    ) {
        let (read_half, mut write_half) = tokio::io::split(&mut stream);
        let mut lines = BufReader::new(read_half).lines();
        let mut session_count = 0usize;
        let mut current_session = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let id = message
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let reply = |result: serde_json::Value| {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
            };
            let out = match method {
                "initialize" => Some(reply(
                    serde_json::json!({"protocolVersion": 1, "agentCapabilities": {}}),
                )),
                "session/new" => {
                    session_count += 1;
                    current_session = format!("fake-sess-{session_count}");
                    sessions.lock().unwrap().push(serde_json::json!({
                        "sessionId": current_session.clone(),
                        "_meta": message["params"]["_meta"].clone(),
                    }));
                    Some(reply(serde_json::json!({"sessionId": current_session})))
                }
                "session/prompt" => {
                    let update = |update: rebon_types::SessionUpdate| {
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": serde_json::to_value(SessionUpdateParams {
                                session_id: current_session.clone(),
                                update,
                            })
                            .unwrap(),
                        })
                        .to_string()
                    };
                    let thinking = update(rebon_types::SessionUpdate::ThinkingDelta {
                        text: "checking auth".into(),
                    });
                    let thinking_end = update(rebon_types::SessionUpdate::ThinkingEnd);
                    let first_chunk = update(rebon_types::SessionUpdate::AgentMessageChunk {
                        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                            text: "pool ".into(),
                            annotations: None,
                        }),
                    });
                    let tool = update(rebon_types::SessionUpdate::ToolCall {
                        tool_call_id: "call-1".into(),
                        title: "Reading auth.rs".into(),
                        kind: rebon_types::ToolKind::Read,
                        status: rebon_types::ToolCallStatus::InProgress,
                        content: None,
                        locations: None,
                        raw_input: Some(HashMap::from([(
                            "file_path".into(),
                            serde_json::json!("auth.rs"),
                        )])),
                        raw_output: None,
                    });
                    let tool_update = update(rebon_types::SessionUpdate::ToolCallUpdate {
                        tool_call_id: "call-1".into(),
                        status: Some(rebon_types::ToolCallStatus::Completed),
                        title: None,
                        content: None,
                        locations: None,
                        raw_output: Some(HashMap::from([("lines".into(), serde_json::json!(42))])),
                    });
                    let second_chunk = update(rebon_types::SessionUpdate::AgentMessageChunk {
                        content: rebon_types::ContentBlock::Text(rebon_types::TextContent {
                            text: "answer".into(),
                            annotations: None,
                        }),
                    });
                    for update in [
                        thinking,
                        thinking_end,
                        first_chunk,
                        tool,
                        tool_update,
                        second_chunk,
                    ] {
                        let _ = write_half.write_all(update.as_bytes()).await;
                        let _ = write_half.write_all(b"\n").await;
                    }
                    Some(reply(serde_json::json!({"stopReason": "end_turn"})))
                }
                _ => None,
            };
            if let Some(out) = out {
                let _ = write_half.write_all(out.as_bytes()).await;
                let _ = write_half.write_all(b"\n").await;
            }
        }
    }

    struct FakeConnector {
        connects: AtomicUsize,
        sessions: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[async_trait]
    impl AgentConnector for FakeConnector {
        async fn connect(
            &self,
            delegate: Arc<dyn ClientDelegate>,
        ) -> Result<AcpClient, ClientError> {
            self.connects.fetch_add(1, Ordering::Relaxed);
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            tokio::spawn(serve_fake_agent(theirs, self.sessions.clone()));
            let (reader, writer) = tokio::io::split(ours);
            AcpClient::connect_over_pipe(
                reader,
                writer,
                None,
                "fake-agent",
                default_client_capabilities(),
                delegate,
            )
            .await
        }

        fn label(&self) -> String {
            "fake-agent".to_string()
        }
    }

    struct FailingConnector {
        connects: AtomicUsize,
    }

    #[async_trait]
    impl AgentConnector for FailingConnector {
        async fn connect(
            &self,
            _delegate: Arc<dyn ClientDelegate>,
        ) -> Result<AcpClient, ClientError> {
            self.connects.fetch_add(1, Ordering::Relaxed);
            Err(ClientError::Connection(ConnectionError::Write(
                "intentional test failure".into(),
            )))
        }

        fn label(&self) -> String {
            "failing-agent".to_string()
        }
    }

    async fn active_task_count(pool: &AcpSubAgentPool) -> usize {
        *pool
            .entries
            .get("claudecode")
            .expect("declared agent")
            .active_tasks
            .lock()
            .await
    }

    fn pool_with_fake_agent() -> (
        Arc<AcpSubAgentPool>,
        Arc<FakeConnector>,
        Arc<Mutex<Vec<serde_json::Value>>>,
    ) {
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let connector = Arc::new(FakeConnector {
            connects: AtomicUsize::new(0),
            sessions: sessions.clone(),
        });
        let pool = AcpSubAgentPool::new(
            vec![declared("claudecode")],
            "/tmp/pool".into(),
            vec![PathBuf::from("/tmp/pool")],
            None,
        );
        // The declared sessionMeta rides on the injected backend too,
        // mirroring what `backend_for` would wire for a real process.
        let backend = Arc::new(
            AcpAgentBackend::with_connector(
                "claudecode",
                connector.clone(),
                Arc::new(rebon_acp_client::DirectHostFs),
                Arc::new(rebon_acp_client::NoopTurnJournal),
                Vec::new(),
            )
            .with_session_meta(declared("claudecode").session_meta),
        );
        pool.inject_backend_for_test("claudecode", backend);
        (pool, connector, sessions)
    }

    fn task_request(agent_id: &str, session: &str, hint: Option<&str>) -> ExternalTaskRequest {
        ExternalTaskRequest {
            agent_id: agent_id.to_string(),
            model_hint: hint.map(str::to_string),
            task_session_id: session.to_string(),
            prompt: "do the task".to_string(),
            cwd: None,
            cancel: PromptCancel::new(),
            progress: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn sequential_tasks_restart_the_idle_process_and_never_reuse_a_session() {
        let (pool, connector, sessions) = pool_with_fake_agent();

        let first = pool
            .run_task(task_request(
                "claudecode",
                "subagent-a",
                Some("claude-opus-5"),
            ))
            .await
            .expect("first task");
        let second = pool
            .run_task(task_request("claudecode", "subagent-b", None))
            .await
            .expect("second task");

        assert_eq!(connector.connects.load(Ordering::Relaxed), 2);
        assert_eq!(first.status, ExternalTaskStatus::Completed);
        assert_eq!(first.final_text, "pool answer");
        assert_eq!(first.tool_call_count, 1);
        assert_eq!(first.acp_session_id.as_deref(), Some("fake-sess-1"));
        assert_eq!(second.acp_session_id.as_deref(), Some("fake-sess-1"));

        let sessions = sessions.lock().unwrap().clone();
        assert_eq!(sessions.len(), 2, "one fresh agent session per task");
        // The model hint merged over the declared recipe on task one…
        assert_eq!(
            sessions[0]["_meta"]["claudeCode"]["options"]["model"],
            "claude-opus-5"
        );
        assert_eq!(
            sessions[0]["_meta"]["claudeCode"]["options"]["disallowedTools"],
            serde_json::json!(["Write"]),
            "the declared sessionMeta must survive the hint overlay"
        );
        assert_eq!(sessions[0]["_meta"]["modelHint"], "claude-opus-5");
        // …and no hint means the declared meta rides alone.
        assert!(sessions[1]["_meta"]["claudeCode"]["options"]["model"].is_null());
    }

    #[tokio::test]
    async fn overlapping_tasks_keep_the_process_until_the_last_lease_finishes() {
        let (pool, connector, _sessions) = pool_with_fake_agent();
        let first = pool.acquire_task("claudecode").await.expect("first lease");
        let backend = first.backend();
        backend
            .start_session_with_meta(AgentSessionSpec::new("overlap-a", "/tmp/pool"), None)
            .await
            .expect("first session");
        let second = pool.acquire_task("claudecode").await.expect("second lease");

        first.finish().await;
        assert_eq!(active_task_count(&pool).await, 1);
        backend
            .start_session_with_meta(AgentSessionSpec::new("overlap-b", "/tmp/pool"), None)
            .await
            .expect("second session on the live process");
        assert_eq!(connector.connects.load(Ordering::Relaxed), 1);

        second.finish().await;
        assert_eq!(active_task_count(&pool).await, 0);

        let third = pool.acquire_task("claudecode").await.expect("third lease");
        third
            .backend()
            .start_session_with_meta(AgentSessionSpec::new("overlap-c", "/tmp/pool"), None)
            .await
            .expect("session after idle restart");
        assert_eq!(connector.connects.load(Ordering::Relaxed), 2);
        third.finish().await;
    }

    #[tokio::test]
    async fn failed_session_start_releases_the_task_lease() {
        let connector = Arc::new(FailingConnector {
            connects: AtomicUsize::new(0),
        });
        let pool = AcpSubAgentPool::new(
            vec![declared("claudecode")],
            "/tmp/pool".into(),
            vec![PathBuf::from("/tmp/pool")],
            None,
        );
        let backend = Arc::new(AcpAgentBackend::with_connector(
            "claudecode",
            connector.clone(),
            Arc::new(rebon_acp_client::DirectHostFs),
            Arc::new(rebon_acp_client::NoopTurnJournal),
            Vec::new(),
        ));
        pool.inject_backend_for_test("claudecode", backend);

        let first = pool
            .run_task(task_request("claudecode", "failing-a", None))
            .await
            .expect_err("session start must fail");
        assert!(first.contains("could not start a session"));
        assert_eq!(active_task_count(&pool).await, 0);

        pool.run_task(task_request("claudecode", "failing-b", None))
            .await
            .expect_err("the next task retries the connection");
        assert_eq!(connector.connects.load(Ordering::Relaxed), 2);
        assert_eq!(active_task_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn dropping_a_running_task_lease_schedules_idle_cleanup() {
        let (pool, connector, _sessions) = pool_with_fake_agent();
        let lease = pool.acquire_task("claudecode").await.expect("task lease");
        lease
            .backend()
            .start_session_with_meta(AgentSessionSpec::new("dropped-a", "/tmp/pool"), None)
            .await
            .expect("session");
        drop(lease);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if active_task_count(&pool).await == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop cleanup must complete");

        let next = pool.acquire_task("claudecode").await.expect("next lease");
        next.backend()
            .start_session_with_meta(AgentSessionSpec::new("dropped-b", "/tmp/pool"), None)
            .await
            .expect("reconnected session");
        assert_eq!(connector.connects.load(Ordering::Relaxed), 2);
        next.finish().await;

        pool.shutdown().await;
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn task_updates_stream_text_thinking_and_tool_lifecycle_in_order() {
        let (pool, _connector, _sessions) = pool_with_fake_agent();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let mut request = task_request("claudecode", "subagent-live", None);
        request.progress = Some(Arc::new(move |event| {
            sink_events.lock().unwrap().push(event);
        }));

        let outcome = pool.run_task(request).await.expect("live task");

        assert_eq!(outcome.final_text, "pool answer");
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ExternalTaskEvent::Thinking("checking auth".into()),
                ExternalTaskEvent::ThinkingEnd,
                ExternalTaskEvent::AgentText("pool ".into()),
                ExternalTaskEvent::ToolCall {
                    tool_call_id: "call-1".into(),
                    title: "Reading auth.rs".into(),
                    input: Some(serde_json::json!({"file_path": "auth.rs"})),
                },
                ExternalTaskEvent::ToolCallUpdate {
                    tool_call_id: "call-1".into(),
                    title: "Reading auth.rs".into(),
                    status: Some(rebon_types::ToolCallStatus::Completed),
                    output: Some(serde_json::json!({"lines": 42})),
                },
                ExternalTaskEvent::AgentText("answer".into()),
            ]
        );
    }

    #[tokio::test]
    async fn resolve_agent_folds_case_and_reports_the_canonical_spelling() {
        let (pool, _connector, _sessions) = pool_with_fake_agent();
        assert_eq!(
            pool.resolve_agent("  ClaudeCode "),
            Some("claudecode".to_string())
        );
        assert_eq!(pool.resolve_agent("codex"), None);
    }

    #[tokio::test]
    async fn the_auto_responder_allows_once_and_never_always() {
        let (publisher, receiver) = ChannelPermissionRequestPublisher::new();
        let responder = spawn_auto_permission_responder("claudecode".into(), receiver);

        let option = |id: &str, kind: PermissionOptionKind| PermissionOption {
            option_id: id.to_string(),
            name: id.to_string(),
            kind,
        };
        let params = |options: Vec<PermissionOption>| RequestPermissionParams {
            session_id: "sess".into(),
            tool_call: ToolCallReference {
                tool_call_id: "call-1".into(),
            },
            options,
            title: None,
            message: None,
            tool_name: Some("Write".into()),
            tool_input: None,
            metadata: None,
        };

        let picked = publisher
            .request_permission(params(vec![
                option("always", PermissionOptionKind::AllowAlways),
                option("once", PermissionOptionKind::AllowOnce),
                option("no", PermissionOptionKind::RejectOnce),
            ]))
            .await
            .expect("responded");
        assert_eq!(picked.outcome, PermissionOutcome::Selected);
        assert_eq!(picked.option_id.as_deref(), Some("once"));

        // Only a standing grant on offer: never taken, so reject.
        let rejected = publisher
            .request_permission(params(vec![option(
                "always",
                PermissionOptionKind::AllowAlways,
            )]))
            .await
            .expect("responded");
        assert_eq!(rejected.outcome, PermissionOutcome::Cancelled);
        assert_eq!(rejected.option_id, None);

        responder.abort();
    }
}
