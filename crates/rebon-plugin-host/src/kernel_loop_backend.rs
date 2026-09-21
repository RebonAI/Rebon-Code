//! The embedded agent loop as an [`AgentBackend`] — `/kernel <vendor>`.
//!
//! Deliberately NOT ACP-over-a-pipe: the ACP backend's capability answers
//! (`writes_through_host_fs: false`, `reports_usage: false`) are hardcoded
//! truths about a foreign process, and both would be lies here — the loop's
//! tools run through rebon's own engine pipeline (the tool seat) and its usage is
//! right there in the session log. Implementing the backend trait directly
//! keeps every capability answer honest.
//!
//! This module is V8-free: the loop is driven through the
//! [`LoopHost`]/[`LoopHostSpawner`] seam, so the backend and translator serve
//! the plane host — one loop, one host process.
//!
//! Shape mirrors `AcpAgentBackend::prompt`: start (or reuse) the session's
//! loop host, open the journal turn, submit the prompt over the control
//! face, translate the relayed dsh session log into [`SessionUpdate`]s
//! (published live + folded into the journal), and race the turn against
//! the host's [`PromptCancel`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use rebon_acp_client::journal::TurnJournal;
use rebon_agent_core::backend::{
    AgentBackend, AgentBackendError, AgentBackendKind, AgentCapabilities, AgentSessionSpec,
    AgentSessionStart, SessionResumeMode, SteerOutcome,
};
use rebon_agent_core::prompt_executor::{PromptExecutorError, PromptOutcome, PromptRequest};
use rebon_agent_core::publisher::SessionUpdatePublisher;
use rebon_types::{
    ContentBlock, RegularContent, SessionUpdate, StopReason, TextContent, ToolCallContent,
    ToolCallStatus, ToolKind, Usage,
};
use serde_json::Value;

use crate::loop_host::{LoopAgentSpec, LoopHost, LoopHostSpawner};

/// How a `kernel:<vendor>` session resolves its loop composition.
#[derive(Debug, Clone)]
pub struct KernelLoopConfig {
    /// Which loop assembly this backend composes.
    pub vendor: String,
    /// Model route the loop's configured agent declares; an adapter for it
    /// must be among the user's `kernelPlugins` entries.
    pub provider: String,
    pub model: String,
    /// Config dir carrying `kernelPlugins` (adapters, grants).
    pub config_dir: PathBuf,
    /// Optional prompt section for the loop realm's dsh systemPrompt.
    pub prompt_section: Option<Value>,
}

impl KernelLoopConfig {
    /// Parse the vendor's `kernelPlugins.loops.<vendor>` entry
    /// (`{provider, model, section?}`); for `dsh` the legacy
    /// `kernelPlugins.loopAgent` shape remains a fallback alias (no
    /// migration, no warning — `loops.dsh` simply wins when both exist).
    ///
    /// `None` — absent or incomplete — means the host simply does not
    /// offer `kernel:<vendor>`; an explicit-but-broken entry is a silent
    /// absence too, which the `/kernel` list's unconfigured row makes
    /// visible enough for a config the user just wrote.
    pub fn for_vendor(config_dir: &std::path::Path, vendor: &str) -> Option<Self> {
        let raw = std::fs::read(config_dir.join("config.json")).ok()?;
        let config: Value = serde_json::from_slice(&raw).ok()?;
        let plugins = config.get("kernelPlugins")?;
        let entry = plugins
            .get("loops")
            .and_then(|loops| loops.get(vendor))
            .or_else(|| {
                (vendor == "dsh")
                    .then(|| plugins.get("loopAgent"))
                    .flatten()
            })?;
        let provider = entry.get("provider")?.as_str()?.trim().to_string();
        let model = entry.get("model")?.as_str()?.trim().to_string();
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            vendor: vendor.to_string(),
            provider,
            model,
            config_dir: config_dir.to_path_buf(),
            prompt_section: entry.get("section").cloned(),
        })
    }

    /// Legacy entry point: the dsh vendor's config.
    pub fn from_config_dir(config_dir: &std::path::Path) -> Option<Self> {
        Self::for_vendor(config_dir, "dsh")
    }
}

/// How one running turn's stream is routed.
struct ActiveTurn {
    publisher: Option<Arc<dyn SessionUpdatePublisher>>,
    usage: Usage,
    saw_thinking: bool,
    thinking_open: bool,
    errors: Vec<String>,
    done: Option<tokio::sync::oneshot::Sender<TurnEnd>>,
}

struct TurnEnd {
    reason_kind: String,
    error: Option<String>,
}

/// Per-session live state: the host plus the turn router its translator
/// task publishes through.
struct LoopSession {
    handle: Arc<dyn LoopHost>,
    turn: Arc<StdMutex<Option<ActiveTurn>>>,
    /// A `close_session` that arrived while a turn was running. The switch
    /// contract says an in-flight turn keeps the backend it started on, so
    /// the shutdown is held here and honoured when the turn ends.
    close_pending: AtomicBool,
    /// This session's host has been shut down (or is about to be, with no
    /// turn to protect it). Written and read **under the `turn` mutex**,
    /// which is what makes "close it" and "claim it for a turn" one
    /// decision instead of two racing ones.
    closed: AtomicBool,
}

impl LoopSession {
    /// Claim this session for a turn, unless it has already been closed.
    ///
    /// The check and the registration happen under one lock, so a
    /// `close_session` either sees the turn (and defers its shutdown) or
    /// wins outright and leaves a session no turn can start on — never the
    /// in-between where a turn runs against a host being torn down.
    fn claim_turn(&self, turn: ActiveTurn) -> bool {
        let mut slot = self.turn.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        *slot = Some(turn);
        true
    }

    /// Close this turn out, honouring a close that arrived while it ran.
    fn end_turn(&self) -> Option<ActiveTurn> {
        let finished = {
            let mut slot = self.turn.lock().unwrap();
            let finished = slot.take();
            if self.close_pending.load(Ordering::SeqCst) {
                // Under the lock: from here no turn can be claimed on this
                // session, so the shutdown below cannot land on a new one.
                self.closed.store(true, Ordering::SeqCst);
            }
            finished
        };
        if self.close_pending.swap(false, Ordering::SeqCst) {
            self.handle.shutdown();
        }
        finished
    }
}

/// The embedded loop as a session backend.
pub struct KernelLoopBackend {
    name: String,
    spawner: Arc<dyn LoopHostSpawner>,
    config: KernelLoopConfig,
    journal: Arc<dyn TurnJournal>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<LoopSession>>>,
}

impl KernelLoopBackend {
    pub fn new(
        name: impl Into<String>,
        spawner: Arc<dyn LoopHostSpawner>,
        config: KernelLoopConfig,
        journal: Arc<dyn TurnJournal>,
    ) -> Self {
        Self {
            name: name.into(),
            spawner,
            config,
            journal,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn session(
        &self,
        spec: &AgentSessionSpec,
    ) -> Result<(Arc<LoopSession>, SessionResumeMode), AgentBackendError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&spec.session_id) {
            if !session.handle.has_exited() {
                return Ok((session.clone(), SessionResumeMode::Reused));
            }
            sessions.remove(&spec.session_id);
        }
        let handle = self
            .spawner
            .spawn(LoopAgentSpec {
                session_id: spec.session_id.clone(),
                vendor: self.config.vendor.clone(),
                provider: self.config.provider.clone(),
                model: self.config.model.clone(),
                workspace_root: PathBuf::from(&spec.cwd),
                config_dir: self.config.config_dir.clone(),
                prompt_section: self.config.prompt_section.clone(),
            })
            .await
            .map_err(AgentBackendError::Unavailable)?;

        let turn: Arc<StdMutex<Option<ActiveTurn>>> = Arc::new(StdMutex::new(None));
        let events = handle.take_events().ok_or_else(|| {
            AgentBackendError::Unavailable("loop event stream unavailable".into())
        })?;
        tokio::spawn(translate_events(
            events,
            spec.session_id.clone(),
            self.journal.clone(),
            turn.clone(),
        ));

        let session = Arc::new(LoopSession {
            handle,
            turn,
            close_pending: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        sessions.insert(spec.session_id.clone(), session.clone());
        // No persistence backend is mounted, so a stored id cannot be
        // replayed — this is always a fresh dsh session (honest `Created`,
        // never `Loaded`).
        Ok((session, SessionResumeMode::Created))
    }
}

/// Flatten prompt blocks into the loop's text-only inbox message.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text.text);
            }
            other => {
                // Non-text input has no loop-side seat yet; note it
                // rather than dropping it silently.
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[unsupported content block: {other:?}]"));
            }
        }
    }
    out
}

fn text_update(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk {
        content: ContentBlock::Text(TextContent {
            text: text.to_string(),
            annotations: None,
        }),
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        // Two renderer-local calls the policy kinds do not make: Glob reads a
        // listing rather than searching text, and WebSearch draws as a search
        // rather than a fetch.
        "Glob" => ToolKind::Read,
        "WebSearch" => ToolKind::Search,
        _ => rebon_tool::render_tool_kind_for_name(name),
    }
}

/// The dsh tool-result message, decomposed: `createToolResultMessage`
/// wraps one `{type:'tool-result', toolCallId, content, isError}` block in
/// a user-role message whose `source` carries the same call id.
fn tool_result_parts(message: &Value) -> (String, bool, String) {
    let mut call_id = message
        .get("source")
        .and_then(|s| s.get("callId"))
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    let mut is_error = false;
    let mut text = String::new();
    if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool-result") => {
                    if call_id.is_empty() {
                        if let Some(id) = block.get("toolCallId").and_then(|c| c.as_str()) {
                            call_id = id.to_string();
                        }
                    }
                    if block.get("isError").and_then(|e| e.as_bool()) == Some(true) {
                        is_error = true;
                    }
                    if let Some(inner) = block.get("content").and_then(|inner| inner.as_array()) {
                        for b in inner {
                            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                }
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
                _ => {}
            }
        }
    }
    (call_id, is_error, text)
}

fn accumulate_usage(usage: &mut Usage, reported: &Value) {
    let read = |key: &str| reported.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    usage.input_tokens += read("inputTokens");
    usage.output_tokens += read("outputTokens");
    usage.cache_read_input_tokens += read("cacheReadTokens");
    usage.cache_creation_input_tokens += read("cacheWriteTokens");
    usage.reasoning_tokens += read("reasoningTokens");
}

/// Publish one update to the live sink and fold it into the journal —
/// the same double-write the ACP session router performs.
async fn deliver(
    journal: &Arc<dyn TurnJournal>,
    session_id: &str,
    publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    update: SessionUpdate,
) {
    journal.record_update(session_id, &update);
    if let Some(publisher) = publisher {
        publisher.publish_to(&session_id.to_string(), update).await;
    }
}

/// The session's translator: the relayed dsh session log in, host-facing
/// [`SessionUpdate`]s out, for as long as the loop host lives. Runs on the
/// host runtime — the kernel-event listener side only queues (the emit
/// happens synchronously on the compose thread and must never block).
async fn translate_events(
    mut events: tokio::sync::mpsc::UnboundedReceiver<Value>,
    session_id: String,
    journal: Arc<dyn TurnJournal>,
    turn: Arc<StdMutex<Option<ActiveTurn>>>,
) {
    while let Some(event) = events.recv().await {
        if event.get("channel").and_then(|c| c.as_str()) == Some("error") {
            let rendered = event
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown agent error")
                .to_string();
            tracing::warn!(session = %session_id, error = %rendered, "kernel loop agent error");
            if let Some(active) = turn.lock().unwrap().as_mut() {
                active.errors.push(rendered);
            }
            continue;
        }
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        match event_type {
            "assistant/chunk" => {
                let chunk = data.get("chunk").cloned().unwrap_or(Value::Null);
                let publisher = {
                    let mut guard = turn.lock().unwrap();
                    let Some(active) = guard.as_mut() else {
                        continue;
                    };
                    match chunk.get("type").and_then(|t| t.as_str()) {
                        Some("reasoning-delta") => {
                            active.saw_thinking = true;
                            active.thinking_open = true;
                        }
                        Some("text-delta") => {}
                        _ => continue,
                    }
                    active.publisher.clone()
                };
                let text = chunk
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                let update = match chunk.get("type").and_then(|t| t.as_str()) {
                    Some("reasoning-delta") => SessionUpdate::ThinkingDelta { text },
                    _ => text_update(&text),
                };
                deliver(&journal, &session_id, &publisher, update).await;
            }
            "tool/call" => {
                let publisher = match turn.lock().unwrap().as_ref() {
                    Some(active) => active.publisher.clone(),
                    None => continue,
                };
                let call_id = data
                    .get("callId")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_string();
                let name = data
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let raw_input = data
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|parsed| parsed.as_object().cloned())
                    .map(|map| map.into_iter().collect::<HashMap<String, Value>>());
                let update = SessionUpdate::ToolCall {
                    tool_call_id: call_id,
                    title: name.clone(),
                    kind: tool_kind(&name),
                    status: ToolCallStatus::InProgress,
                    content: None,
                    locations: None,
                    raw_input,
                    raw_output: None,
                };
                deliver(&journal, &session_id, &publisher, update).await;
            }
            "tool/result" => {
                let publisher = match turn.lock().unwrap().as_ref() {
                    Some(active) => active.publisher.clone(),
                    None => continue,
                };
                let message = data.get("message").cloned().unwrap_or(Value::Null);
                let (call_id, block_error, text) = tool_result_parts(&message);
                let failed = block_error || data.get("error").is_some();
                let update = SessionUpdate::ToolCallUpdate {
                    tool_call_id: call_id,
                    status: Some(if failed {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    }),
                    title: None,
                    content: (!text.is_empty()).then(|| {
                        vec![ToolCallContent::Content(RegularContent {
                            content: ContentBlock::Text(TextContent {
                                text,
                                annotations: None,
                            }),
                        })]
                    }),
                    locations: None,
                    raw_output: None,
                };
                deliver(&journal, &session_id, &publisher, update).await;
            }
            "assistant/message" => {
                let (publisher, close_thinking) = {
                    let mut guard = turn.lock().unwrap();
                    let Some(active) = guard.as_mut() else {
                        continue;
                    };
                    if let Some(usage) = data.get("usage") {
                        accumulate_usage(&mut active.usage, usage);
                    }
                    let close = active.thinking_open;
                    active.thinking_open = false;
                    (active.publisher.clone(), close)
                };
                if close_thinking {
                    deliver(
                        &journal,
                        &session_id,
                        &publisher,
                        SessionUpdate::ThinkingEnd,
                    )
                    .await;
                }
            }
            "turn/end" => {
                let reason_kind = data
                    .get("reason")
                    .and_then(|r| r.get("kind"))
                    .and_then(|k| k.as_str())
                    .unwrap_or("completed")
                    .to_string();
                let error = data
                    .get("reason")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                if let Some(active) = turn.lock().unwrap().as_mut() {
                    if let Some(done) = active.done.take() {
                        let _ = done.send(TurnEnd { reason_kind, error });
                    }
                }
            }
            _ => {}
        }
    }

    // End of stream: the loop host is gone (stream EOF, host death, a
    // crash between two frames). Nothing will ever send `turn/end` now,
    // and the running prompt still holds this turn's `done` — parked on a
    // receiver that cannot even report the sender dropped, because the
    // sender lives in the ActiveTurn the live session still owns. Release
    // it here, or the turn hangs for as long as the session does.
    let orphaned = {
        let mut guard = turn.lock().unwrap();
        guard.as_mut().and_then(|active| {
            active
                .done
                .take()
                .map(|done| (done, active.errors.last().cloned()))
        })
    };
    if let Some((done, last_error)) = orphaned {
        tracing::warn!(
            session = %session_id,
            "kernel loop event stream ended mid-turn; failing the turn instead of parking it"
        );
        let _ = done.send(TurnEnd {
            reason_kind: "error".to_string(),
            error: Some(
                last_error.unwrap_or_else(|| "the kernel loop host stopped mid-turn".to_string()),
            ),
        });
    }
}

#[async_trait]
impl AgentBackend for KernelLoopBackend {
    fn kind(&self) -> AgentBackendKind {
        AgentBackendKind::Kernel
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            // The loop's writes run through the engine's tool pipeline
            // (validation + permission), but the tool seat does not arm the
            // file-history snapshot pipeline yet — so `/rewind` must not
            // be promised. Honest false until that seam is wired.
            writes_through_host_fs: false,
            // Ungranted non-read-only calls fail closed with a stable code
            // instead of reaching the host's ask UI (that surface is not
            // wired to session publishers yet).
            host_permission_prompts: false,
            // No dsh session-persistence backend is mounted.
            resumable_sessions: false,
            // Usage comes straight from the loop's session log.
            reports_usage: true,
        }
    }

    async fn start_session(
        &self,
        spec: AgentSessionSpec,
    ) -> Result<AgentSessionStart, AgentBackendError> {
        let (session, mode) = self.session(&spec).await?;
        Ok(AgentSessionStart {
            session_id: session.handle.agent_id().to_string(),
            mode,
        })
    }

    async fn prompt(&self, request: PromptRequest) -> Result<PromptOutcome, PromptExecutorError> {
        let host_session_id = request.session_id.clone();
        let spec = AgentSessionSpec {
            session_id: host_session_id.clone(),
            cwd: request.cwd.clone(),
            mcp_servers: request.mcp_servers.clone(),
            additional_working_directories: request.additional_working_directories.clone(),
            resume_session_id: None,
        };
        // Resolving the session and claiming it for this turn have to be one
        // decision. Apart, a `close_session` landing between them finds no
        // turn to protect, shuts the host down, and this turn then runs
        // against a dead one. `claim_turn` refuses on a session that lost
        // that race; the closed session is already out of the map, so the
        // retry resolves a fresh host rather than spinning on the dead one.
        let (session, done_rx) = {
            let mut attempts = 0;
            loop {
                let (session, _mode) = self
                    .session(&spec)
                    .await
                    .map_err(|err| PromptExecutorError::Execution(err.to_string()))?;
                let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                if session.claim_turn(ActiveTurn {
                    publisher: request.update_publisher.clone(),
                    usage: Usage::default(),
                    saw_thinking: false,
                    thinking_open: false,
                    errors: Vec::new(),
                    done: Some(done_tx),
                }) {
                    break (session, done_rx);
                }
                attempts += 1;
                if attempts >= 3 {
                    return Err(PromptExecutorError::Execution(
                        "the kernel loop session kept closing under this turn".into(),
                    ));
                }
            }
        };

        // The journal turn opens after the stream is routed — same ordering
        // discipline as the ACP backend (no orphan prompt rows).
        let turn_id = self.journal.begin_turn(&host_session_id, &request.prompt);

        let close_turn = |stop: Option<StopReason>| {
            if let Some(turn_id) = &turn_id {
                self.journal.end_turn(&host_session_id, turn_id, stop);
            }
        };

        let submitted = session.handle.followup(&prompt_text(&request.prompt)).await;
        if let Err(err) = submitted {
            session.end_turn();
            close_turn(None);
            return Err(PromptExecutorError::Execution(err));
        }

        // The turn ends when the loop says so; a local cancel is forwarded
        // and then we keep waiting — the loop's aborted turn/end is the
        // authoritative close (the ACP select, transcribed).
        tokio::pin!(done_rx);
        let end = tokio::select! {
            end = &mut done_rx => end,
            () = request.cancel.notified() => {
                if let Err(err) = session.handle.cancel().await {
                    tracing::warn!(error = %err, "kernel loop: cancel could not be delivered");
                    session.end_turn();
                    close_turn(Some(StopReason::Cancelled));
                    return Err(PromptExecutorError::Cancelled);
                }
                (&mut done_rx).await
            }
        };

        let finished = session.end_turn();
        let usage = finished
            .as_ref()
            .map(|f| f.usage.clone())
            .unwrap_or_default();
        let end = match end {
            Ok(end) => end,
            Err(_translator_gone) => {
                close_turn(None);
                return Err(PromptExecutorError::Execution(
                    "kernel loop event stream closed mid-turn".into(),
                ));
            }
        };

        match end.reason_kind.as_str() {
            "completed" | "blocked" => {
                close_turn(Some(StopReason::EndTurn));
                Ok(PromptOutcome {
                    stop_reason: StopReason::EndTurn,
                    usage,
                })
            }
            "max-tokens" => {
                close_turn(Some(StopReason::MaxTokens));
                Ok(PromptOutcome {
                    stop_reason: StopReason::MaxTokens,
                    usage,
                })
            }
            "aborted" => {
                close_turn(Some(StopReason::Cancelled));
                if request.cancel.is_cancelled() {
                    Err(PromptExecutorError::Cancelled)
                } else {
                    Ok(PromptOutcome {
                        stop_reason: StopReason::Cancelled,
                        usage,
                    })
                }
            }
            _error => {
                close_turn(None);
                let detail = end
                    .error
                    .or_else(|| finished.and_then(|f| f.errors.into_iter().next_back()))
                    .unwrap_or_else(|| "the loop turn failed".into());
                Err(PromptExecutorError::Execution(detail))
            }
        }
    }

    async fn cancel(&self, session_id: &str) -> Result<(), AgentBackendError> {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id).cloned()
        };
        let Some(session) = session else {
            return Ok(());
        };
        session
            .handle
            .cancel()
            .await
            .map_err(AgentBackendError::Transport)
    }

    async fn steer(
        &self,
        session_id: &str,
        blocks: Vec<ContentBlock>,
        user_message_uuid: &str,
    ) -> Result<SteerOutcome, AgentBackendError> {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id).cloned()
        };
        let Some(session) = session else {
            return Err(AgentBackendError::UnknownSession(session_id.to_string()));
        };
        // Only a running turn can absorb a steer; an idle loop would treat
        // it as a fresh wake, stealing the caller's queue-or-prompt choice.
        let status = session
            .handle
            .status()
            .await
            .map_err(AgentBackendError::Transport)?;
        if status != "running" {
            return Ok(SteerOutcome::TurnAlreadyOver);
        }
        self.journal
            .record_steered_prompt(session_id, user_message_uuid, &blocks);
        session
            .handle
            .steer(&prompt_text(&blocks))
            .await
            .map_err(AgentBackendError::Transport)?;
        Ok(SteerOutcome::Injected)
    }

    async fn close_session(&self, session_id: &str) -> Result<(), AgentBackendError> {
        let session = { self.sessions.lock().await.remove(session_id) };
        if let Some(session) = session {
            // `AgentBackendSwitch` promises an in-flight turn keeps running
            // on the backend it started on, and `/kernel <vendor>` closes
            // the backend it just walked away from. Shutting the host down
            // here would kill exactly the turn that promise protects, so a
            // running turn defers the close to its own end instead. The
            // decision is taken under the turn lock so it cannot race
            // `end_turn` and strand the host either way.
            let deferred = {
                let turn = session.turn.lock().unwrap();
                if turn.is_some() {
                    session.close_pending.store(true, Ordering::SeqCst);
                    true
                } else {
                    // Under the lock, so a turn cannot be claimed between
                    // this decision and the shutdown below.
                    session.closed.store(true, Ordering::SeqCst);
                    false
                }
            };
            if !deferred {
                session.handle.shutdown();
            }
        }
        Ok(())
    }
}
