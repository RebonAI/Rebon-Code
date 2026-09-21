use super::super::*;
use rebon_core::query::AttachmentPollRequest;

/// Outcome of a worker's attempt to take exclusive ownership of a job.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkerClaim {
    /// This worker now owns the job; the returned state has its pid.
    Claimed(Box<BackgroundJobState>),
    /// The job left Queued before this worker acquired it.
    NoWork,
    /// Another live worker owns the job; this worker must exit.
    OwnedByOther(u32),
    /// The job was stopped; nothing to do.
    Stopped,
}

pub(crate) fn parent_published_this_worker(
    owner: &RecordedOwnerSnapshot,
    snapshot_generation: u64,
    my_pid: u32,
    my_identity: &Option<String>,
    my_detached_group: bool,
) -> bool {
    *owner
        == RecordedOwnerSnapshot::owned(
            my_pid,
            my_identity.clone(),
            my_detached_group,
            false,
            None,
            None,
            snapshot_generation,
        )
}

/// Atomically claim `job_id` for `my_pid`.
///
/// Only one worker may drive a job at a time. The supervisor's spawn
/// tick and an existing worker's reuse loop can both react to the same
/// Queued job within one poll interval; without this gate each of them
/// runs a full session — duplicate MCP/analyzer stacks and contention
/// on the session file (os error 33).
pub(crate) fn claim_background_job(
    store: &BackgroundStore,
    job_id: &str,
    my_pid: u32,
    ipc_port: u16,
    ipc_token: &str,
    is_running: impl Fn(u32) -> Option<bool>,
) -> anyhow::Result<WorkerClaim> {
    let snapshot = store.read_state(job_id)?;
    if snapshot.process.status == BackgroundJobStatus::Stopped {
        return Ok(WorkerClaim::Stopped);
    }
    if snapshot.process.process_owner_fenced {
        return Ok(WorkerClaim::NoWork);
    }
    // Liveness is checked outside the store lock (it shells out on
    // Windows). `known_dead` lets the claim overwrite exactly the pid
    // that was seen dead while still yielding to any pid written in
    // between by another claimer.
    let snapshot_owner = snapshot.recorded_owner();
    let known_dead = snapshot_owner.pid.filter(|&pid| {
        if pid == my_pid || snapshot_owner.owner_detached_group {
            return false;
        }
        match snapshot_owner.pid_identity.as_deref() {
            Some(identity) => matches!(
                rebon_session_host::recorded_process_is_running(pid, Some(identity)),
                Ok(false)
            ),
            None => is_running(pid) == Some(false),
        }
    });
    if let Some(other) = snapshot.process.pid {
        if other != my_pid && known_dead != Some(other) {
            return Ok(WorkerClaim::OwnedByOther(other));
        }
    }
    if snapshot.process.status != BackgroundJobStatus::Queued {
        return Ok(WorkerClaim::NoWork);
    }
    let my_identity = rebon_session_host::process_identity(my_pid);
    let my_detached_group = process_owns_detached_group(my_pid);
    store.update_state(job_id, |current| {
        if current.process.status == BackgroundJobStatus::Stopped {
            return Ok(WorkerClaim::Stopped);
        }
        if current.process.process_owner_fenced {
            return Ok(WorkerClaim::NoWork);
        }
        let current_owner = current.recorded_owner();
        if current_owner != snapshot_owner
            && !parent_published_this_worker(
                &current_owner,
                snapshot_owner.turn_generation,
                my_pid,
                &my_identity,
                my_detached_group,
            )
        {
            return Ok(match current_owner.pid {
                Some(other) if other != my_pid => WorkerClaim::OwnedByOther(other),
                _ => WorkerClaim::NoWork,
            });
        }
        if let Some(other) = current_owner.pid {
            if other != my_pid && known_dead != Some(other) {
                return Ok(WorkerClaim::OwnedByOther(other));
            }
        }
        if current.process.status != BackgroundJobStatus::Queued {
            return Ok(WorkerClaim::NoWork);
        }
        let next_generation = current_owner.turn_generation.wrapping_add(1).max(1);
        current.set_recorded_owner(RecordedOwnerSnapshot::owned(
            my_pid,
            my_identity.clone(),
            my_detached_group,
            false,
            Some(ipc_port),
            Some(ipc_token.to_string()),
            next_generation,
        ));
        current.process.status = BackgroundJobStatus::Running;
        for prompt in &mut current.identity.pending_prompts {
            prompt.claimed_turn_generation = None;
            prompt.completed_turn_generation = None;
        }
        if let Some(prompt) = current.identity.pending_prompts.first_mut() {
            prompt.claimed_turn_generation = Some(current.process.turn_generation);
        }
        if current.process.started_at_ms.is_none() {
            current.process.started_at_ms = Some(now_ms());
        }
        current.process.updated_at_ms = now_ms();
        // A claimed queue head remains durable until finalization. If the
        // worker exits, the next generation can reclaim the same ID and use
        // the transcript UUID to resume without duplicating the user row.
        Ok(WorkerClaim::Claimed(Box::new(current.clone())))
    })
}

pub(crate) fn claimed_pending_prompts(state: &BackgroundJobState) -> &[PendingPrompt] {
    let claimed = state
        .identity
        .pending_prompts
        .iter()
        .take_while(|prompt| prompt.claimed_turn_generation == Some(state.process.turn_generation))
        .count();
    &state.identity.pending_prompts[..claimed]
}

pub(crate) fn claimed_pending_prompt(state: &BackgroundJobState) -> Option<&PendingPrompt> {
    claimed_pending_prompts(state).first()
}

#[derive(Debug)]
pub(crate) struct BackgroundPendingPromptPoller {
    store: BackgroundStore,
    job_id: String,
    turn_generation: u64,
    pid_identity: Option<String>,
    ipc_port: u16,
    ipc_token: String,
    coordinator_report_paths: Mutex<std::collections::HashMap<String, Vec<PathBuf>>>,
}

impl BackgroundPendingPromptPoller {
    pub(crate) fn new(
        store: BackgroundStore,
        state: &BackgroundJobState,
        ipc: &BackgroundIpcServer,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            job_id: state.identity.job_id.clone(),
            turn_generation: state.process.turn_generation,
            pid_identity: ipc.pid_identity.clone(),
            ipc_port: ipc.port,
            ipc_token: ipc.token.clone(),
            coordinator_report_paths: Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub(crate) fn owns_turn(&self, state: &BackgroundJobState, session_id: &str) -> bool {
        let expected_owner = RecordedOwnerSnapshot::owned(
            std::process::id(),
            self.pid_identity.clone(),
            process_owns_detached_group(std::process::id()),
            false,
            Some(self.ipc_port),
            Some(self.ipc_token.clone()),
            self.turn_generation,
        );
        state.recorded_owner() == expected_owner
            && state.identity.session_id.as_deref() == Some(session_id)
            && matches!(
                state.process.status,
                BackgroundJobStatus::Running | BackgroundJobStatus::NeedsInput
            )
    }

    pub(crate) fn claim_pending_prompts(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<PendingPrompt>> {
        let claimed_at = now_ms();
        self.store.update_state(&self.job_id, |state| {
            if !self.owns_turn(state, session_id) {
                return Ok(Vec::new());
            }
            let first_unclaimed = state
                .identity
                .pending_prompts
                .iter()
                .position(|prompt| prompt.claimed_turn_generation.is_none())
                .unwrap_or(state.identity.pending_prompts.len());
            if first_unclaimed == state.identity.pending_prompts.len() {
                return Ok(Vec::new());
            }
            let claimed = state.identity.pending_prompts[first_unclaimed..]
                .iter_mut()
                .map(|prompt| {
                    prompt.claimed_turn_generation = Some(self.turn_generation);
                    prompt.clone()
                })
                .collect();
            state.process.updated_at_ms = claimed_at;
            Ok(claimed)
        })
    }

    pub(crate) fn claim_next_pending_prompt(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<PendingPrompt>> {
        let claimed_at = now_ms();
        self.store.update_state(&self.job_id, |state| {
            if !self.owns_turn(state, session_id) {
                return Ok(None);
            }
            let Some(prompt) = state
                .identity
                .pending_prompts
                .iter_mut()
                .find(|prompt| prompt.claimed_turn_generation.is_none())
            else {
                return Ok(None);
            };
            prompt.claimed_turn_generation = Some(self.turn_generation);
            let claimed = prompt.clone();
            state.process.updated_at_ms = claimed_at;
            Ok(Some(claimed))
        })
    }

    pub(crate) fn release_pending_prompt_claim(
        &self,
        session_id: &str,
        prompt_id: &str,
    ) -> anyhow::Result<()> {
        let released_at = now_ms();
        self.store.update_state(&self.job_id, |state| {
            if !self.owns_turn(state, session_id) {
                return Ok(());
            }
            let Some(prompt) = state
                .identity
                .pending_prompts
                .iter_mut()
                .find(|prompt| prompt.id == prompt_id)
            else {
                return Ok(());
            };
            if prompt.claimed_turn_generation == Some(self.turn_generation) {
                prompt.claimed_turn_generation = None;
                prompt.completed_turn_generation = None;
                state.process.updated_at_ms = released_at;
            }
            Ok(())
        })
    }
}

pub(crate) struct BackgroundSteerPumpHandle {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl BackgroundSteerPumpHandle {
    pub(crate) async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

pub(crate) fn spawn_background_steer_pump(
    agents: Arc<rebon_agent_core::routing::SessionAgents<rebon_acp_client::AcpAgentBackend>>,
    poller: Arc<BackgroundPendingPromptPoller>,
    update_publisher: Arc<dyn rebon_agent_core::SessionUpdatePublisher>,
    session_id: String,
) -> BackgroundSteerPumpHandle {
    let (stop, mut stop_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(BACKGROUND_STEER_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = &mut stop_rx => return,
                _ = interval.tick() => {}
            }
            let prompt = match poller.claim_next_pending_prompt(&session_id) {
                Ok(Some(prompt)) => prompt,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        job_id = %poller.job_id,
                        turn_generation = poller.turn_generation,
                        %error,
                        "background worker could not claim a pending prompt for steering"
                    );
                    return;
                }
            };
            if !prompt.coordinator_report_paths.is_empty() {
                let _ = poller.release_pending_prompt_claim(&session_id, &prompt.id);
                return;
            }
            match agents
                .steer(
                    &session_id,
                    pending_prompt_steer_blocks(&prompt),
                    &prompt.id,
                )
                .await
            {
                Ok(rebon_agent_core::SteerOutcome::Injected) => {
                    publish_steered_pending_prompt(&update_publisher, &session_id, &prompt).await;
                }
                Ok(rebon_agent_core::SteerOutcome::TurnAlreadyOver) | Err(_) => {
                    if let Err(error) = poller.release_pending_prompt_claim(&session_id, &prompt.id)
                    {
                        tracing::warn!(
                            job_id = %poller.job_id,
                            prompt_id = %prompt.id,
                            %error,
                            "background worker could not release an undelivered steering claim"
                        );
                    }
                    return;
                }
            }
        }
    });
    BackgroundSteerPumpHandle { stop, task }
}

pub(crate) fn pending_prompt_steer_blocks(
    prompt: &PendingPrompt,
) -> Vec<rebon_types::ContentBlock> {
    let mut blocks = vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
        text: prompt.text.clone(),
        annotations: None,
    })];
    blocks.extend(
        prompt
            .images
            .iter()
            .map(BackgroundImageAttachment::to_content_block),
    );
    blocks
}

pub(crate) async fn publish_steered_pending_prompt(
    publisher: &Arc<dyn rebon_agent_core::SessionUpdatePublisher>,
    session_id: &str,
    prompt: &PendingPrompt,
) {
    let image_paste_ids = prompt
        .images
        .iter()
        .map(|image| image.id)
        .collect::<Vec<_>>();
    publisher
        .publish_to(
            &session_id.to_string(),
            rebon_types::SessionUpdate::QueuedUserMessage {
                uuid: prompt.id.clone(),
                content: vec![rebon_types::ContentBlock::Text(rebon_types::TextContent {
                    text: prompt.text.clone(),
                    annotations: None,
                })],
                image_paste_ids: (!image_paste_ids.is_empty()).then_some(image_paste_ids),
            },
        )
        .await;
}

pub(crate) fn pending_prompt_attachment_message(prompt: &PendingPrompt) -> rebon_api::Message {
    let mut content = vec![rebon_api::ContentBlock::Text(rebon_api::TextBlock {
        text: prompt.text.clone(),
    })];
    content.extend(prompt.images.iter().map(|image| {
        rebon_api::ContentBlock::Image(rebon_api::ImageBlock::base64(
            image.media_type.clone(),
            image.data.clone(),
        ))
    }));
    let image_ids = prompt
        .images
        .iter()
        .map(|image| image.id.to_string())
        .collect::<Vec<_>>();
    let attrs = if image_ids.is_empty() {
        format!("uuid=\"{}\"", prompt.id)
    } else {
        format!(
            "uuid=\"{}\" imagePasteIds=\"{}\"",
            prompt.id,
            image_ids.join(",")
        )
    };
    content.push(rebon_api::ContentBlock::Text(rebon_api::TextBlock {
        text: format!("<rebon-queued-user-input {attrs} />"),
    }));
    rebon_api::Message {
        role: rebon_api::Role::User,
        content,
    }
}

impl AttachmentPoller for BackgroundPendingPromptPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<rebon_api::Message> {
        match self.claim_pending_prompts(request.session_id) {
            Ok(prompts) => {
                for prompt in &prompts {
                    if !prompt.coordinator_report_paths.is_empty() {
                        self.coordinator_report_paths
                            .lock()
                            .expect("background pending prompt report paths poisoned")
                            .entry(request.turn_id.to_string())
                            .or_default()
                            .extend(prompt.coordinator_report_paths.iter().map(PathBuf::from));
                    }
                    let _ = self.store.append_event(
                        &self.job_id,
                        "pending_prompt_injected",
                        serde_json::json!({
                            "promptId": &prompt.id,
                            "turnGeneration": self.turn_generation,
                            "iteration": request.next_iteration,
                        }),
                    );
                }
                prompts
                    .iter()
                    .map(pending_prompt_attachment_message)
                    .collect()
            }
            Err(error) => {
                tracing::warn!(
                    job_id = %self.job_id,
                    turn_generation = self.turn_generation,
                    %error,
                    "background worker could not claim a pending prompt for mid-turn injection"
                );
                Vec::new()
            }
        }
    }

    fn take_coordinator_report_paths_for_query(
        &self,
        _session_id: &str,
        turn_id: &str,
    ) -> Vec<PathBuf> {
        self.coordinator_report_paths
            .lock()
            .expect("background pending prompt report paths poisoned")
            .remove(turn_id)
            .unwrap_or_default()
    }

    fn finish_turn_for_query(&self, _session_id: &str, turn_id: &str, _succeeded: bool) {
        self.coordinator_report_paths
            .lock()
            .expect("background pending prompt report paths poisoned")
            .remove(turn_id);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingPromptTranscriptState {
    Missing,
    ResumeSavedUser,
    ResumeInterruptedToolUse {
        assistant_uuid: String,
        parent_uuid: String,
        missing_tool_uses: Vec<(String, String)>,
    },
    Completed,
}

impl PendingPromptTranscriptState {
    /// One-line diagnosis for the completion fail-safes: which piece of
    /// durable evidence is missing for the claimed prompt(s). The error
    /// that quotes this is often all a field report contains, so name
    /// the gap precisely instead of leaving a bare "not durable".
    pub(crate) fn durability_gap(&self) -> String {
        match self {
            Self::Missing => "no durable user row for the claimed prompt".to_string(),
            Self::ResumeSavedUser => {
                "the user row is durable, but no assistant response completed the turn after it"
                    .to_string()
            }
            Self::ResumeInterruptedToolUse {
                missing_tool_uses, ..
            } => {
                let names = missing_tool_uses
                    .iter()
                    .map(|(id, name)| {
                        if name.is_empty() {
                            id.as_str()
                        } else {
                            name.as_str()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "the last assistant response left {} tool call(s) without results ({names})",
                    missing_tool_uses.len()
                )
            }
            Self::Completed => "the transcript already records a completed turn".to_string(),
        }
    }
}

pub(crate) fn transcript_user_entry_is_turn_boundary(
    entry: &rebon_session::TranscriptEntry,
) -> bool {
    if entry.entry_type != "user"
        || entry
            .raw
            .get("isMeta")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || entry
            .raw
            .get("runtimeContext")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || entry
            .raw
            .get("isVisibleInTranscriptOnly")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    {
        return false;
    }
    let Some(content) = entry.raw.pointer("/message/content") else {
        return true;
    };
    let Some(blocks) = content.as_array() else {
        return true;
    };
    !blocks.iter().all(|block| {
        block
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind == "tool_result")
    })
}

pub(crate) fn transcript_assistant_completes_turn(entry: &rebon_session::TranscriptEntry) -> bool {
    if entry.entry_type != "assistant" {
        return false;
    }
    match entry
        .raw
        .pointer("/message/stop_reason")
        .or_else(|| entry.raw.pointer("/message/stopReason"))
        .and_then(serde_json::Value::as_str)
    {
        Some("tool_use") => false,
        Some(_) => true,
        None => entry
            .raw
            .pointer("/message/content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|blocks| {
                !blocks.iter().any(|block| {
                    block
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| kind == "tool_use")
                })
            }),
    }
}

fn transcript_tool_uses(entry: &rebon_session::TranscriptEntry) -> Vec<(String, String)> {
    if entry.entry_type != "assistant" {
        return Vec::new();
    }
    entry
        .raw
        .pointer("/message/content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
                return None;
            }
            Some((
                block.get("id")?.as_str()?.to_string(),
                block
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect()
}

fn transcript_tool_result_ids(
    entries: &[&rebon_session::TranscriptEntry],
) -> std::collections::HashSet<String> {
    entries
        .iter()
        .filter(|entry| entry.entry_type == "user")
        .flat_map(|entry| {
            entry
                .raw
                .pointer("/message/content")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|block| {
            (block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result"))
                .then(|| block.get("tool_use_id")?.as_str().map(str::to_string))
                .flatten()
        })
        .collect()
}

fn resume_state_for_incomplete_turn(
    messages: &[&rebon_session::TranscriptEntry],
    last_prompt_index: usize,
) -> PendingPromptTranscriptState {
    let mut turn_end = messages.len();
    for (index, later) in messages.iter().enumerate().skip(last_prompt_index + 1) {
        if transcript_user_entry_is_turn_boundary(later) {
            turn_end = index;
            break;
        }
        if transcript_assistant_completes_turn(later) {
            return PendingPromptTranscriptState::Completed;
        }
    }

    let Some((assistant_index, assistant, tool_uses)) = messages[last_prompt_index + 1..turn_end]
        .iter()
        .enumerate()
        .filter_map(|(offset, entry)| {
            let tool_uses = transcript_tool_uses(entry);
            (!tool_uses.is_empty()).then_some((last_prompt_index + 1 + offset, *entry, tool_uses))
        })
        .last()
    else {
        return PendingPromptTranscriptState::ResumeSavedUser;
    };
    let existing_results = transcript_tool_result_ids(&messages[assistant_index + 1..turn_end]);
    let missing_tool_uses = tool_uses
        .into_iter()
        .filter(|(id, _)| !existing_results.contains(id))
        .collect::<Vec<_>>();
    if missing_tool_uses.is_empty() {
        return PendingPromptTranscriptState::ResumeSavedUser;
    }
    let parent_uuid = messages
        .get(turn_end.saturating_sub(1))
        .unwrap_or(&assistant)
        .uuid
        .clone();
    PendingPromptTranscriptState::ResumeInterruptedToolUse {
        assistant_uuid: assistant.uuid.clone(),
        parent_uuid,
        missing_tool_uses,
    }
}

pub(crate) fn interrupted_tool_result_entry(
    state: &PendingPromptTranscriptState,
) -> Option<rebon_session::TranscriptWriteEntry> {
    let PendingPromptTranscriptState::ResumeInterruptedToolUse {
        assistant_uuid,
        parent_uuid,
        missing_tool_uses,
    } = state
    else {
        return None;
    };
    let content = missing_tool_uses
        .iter()
        .map(|(id, name)| {
            serde_json::json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": rebon_api::context_prune::synthetic_tool_result_content(name),
                "is_error": true,
            })
        })
        .collect::<Vec<_>>();
    Some(
        rebon_session::TranscriptWriteEntry::new(
            "user",
            serde_json::json!({
                "message": {
                    "role": "user",
                    "content": content,
                }
            }),
        )
        .with_uuid(format!("u-background-tool-recovery-{assistant_uuid}"))
        .with_parent(parent_uuid.clone()),
    )
}

pub(crate) fn persist_interrupted_tool_results(
    state: &BackgroundJobState,
    transcript_state: &PendingPromptTranscriptState,
) -> anyhow::Result<usize> {
    let Some(entry) = interrupted_tool_result_entry(transcript_state) else {
        return Ok(0);
    };
    let session_id = state.identity.session_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!("cannot repair tool results without a background session")
    })?;
    let cwd = background_job_transcript_cwd(state);
    rebon_session::append_transcript_entry(
        &rebon_session::default_projects_root(),
        &cwd,
        session_id,
        entry,
    )
    .with_context(|| {
        format!("failed to persist interrupted tool results for background session {session_id}")
    })?;
    let PendingPromptTranscriptState::ResumeInterruptedToolUse {
        missing_tool_uses, ..
    } = transcript_state
    else {
        unreachable!("repair entry only exists for interrupted tool use")
    };
    Ok(missing_tool_uses.len())
}

pub(crate) fn pending_prompts_transcript_state_in_messages(
    messages: &[rebon_session::TranscriptEntry],
    prompt_ids: &[&str],
) -> PendingPromptTranscriptState {
    let canonical_indices = rebon_session::session_storage::reconstruct_chain_indices(messages);
    let canonical = canonical_indices
        .iter()
        .map(|&index| &messages[index])
        .collect::<Vec<_>>();
    let mut search_from = 0;
    let mut last_prompt_index = None;
    for prompt_id in prompt_ids {
        let Some(offset) = canonical[search_from..]
            .iter()
            .position(|entry| entry.uuid == *prompt_id)
        else {
            return if last_prompt_index.is_some() {
                PendingPromptTranscriptState::ResumeSavedUser
            } else {
                PendingPromptTranscriptState::Missing
            };
        };
        let index = search_from + offset;
        last_prompt_index = Some(index);
        search_from = index + 1;
    }
    let Some(last_prompt_index) = last_prompt_index else {
        return PendingPromptTranscriptState::Missing;
    };
    resume_state_for_incomplete_turn(&canonical, last_prompt_index)
}

/// The single-prompt shape of the check above, kept for the tests that
/// exercise the transcript rules one prompt at a time; the worker itself
/// asks about every claimed prompt at once.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn pending_prompt_transcript_state_in_messages(
    messages: &[rebon_session::TranscriptEntry],
    prompt_id: &str,
) -> PendingPromptTranscriptState {
    pending_prompts_transcript_state_in_messages(messages, &[prompt_id])
}

pub(crate) fn pending_prompt_transcript_state(
    state: &BackgroundJobState,
) -> PendingPromptTranscriptState {
    let claimed = claimed_pending_prompts(state);
    if claimed.is_empty() {
        return PendingPromptTranscriptState::Missing;
    }
    let Some(session_id) = state.identity.session_id.as_deref() else {
        return PendingPromptTranscriptState::Missing;
    };
    let cwd = background_job_transcript_cwd(state);
    let path = rebon_session::transcript_file_path(
        &rebon_session::default_projects_root(),
        &cwd,
        session_id,
    );
    let Some(loaded) = rebon_session::load_transcript_from_file(&path)
        .ok()
        .flatten()
    else {
        return PendingPromptTranscriptState::Missing;
    };
    let prompt_ids = claimed
        .iter()
        .map(|prompt| prompt.id.as_str())
        .collect::<Vec<_>>();
    pending_prompts_transcript_state_in_messages(&loaded.messages, &prompt_ids)
}
