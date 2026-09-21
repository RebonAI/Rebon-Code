use super::*;

pub(super) fn build_tool_call_title(name: &str, input: &serde_json::Map<String, Value>) -> String {
    match name {
        "Grep" => match (input_str(input, "pattern"), input_str(input, "path")) {
            (Some(pattern), Some(path)) => format!("Grep \"{pattern}\" in {path}"),
            (Some(pattern), None) => format!("Grep \"{pattern}\""),
            _ => "Grep".into(),
        },
        "Glob" => match (input_str(input, "pattern"), input_str(input, "path")) {
            (Some(pattern), Some(path)) => format!("Glob {pattern} in {path}"),
            (Some(pattern), None) => format!("Glob {pattern}"),
            _ => "Glob".into(),
        },
        "Read" => input_str(input, "file_path")
            .map(|v| format!("Read {v}"))
            .unwrap_or_else(|| "Read".into()),
        "Edit" => input_str(input, "file_path")
            .map(|v| format!("Edit {v}"))
            .unwrap_or_else(|| "Edit".into()),
        "MultiEdit" => input_str(input, "file_path")
            .map(|v| {
                let count = input
                    .get("edits")
                    .and_then(|edits| edits.as_array())
                    .map(Vec::len)
                    .unwrap_or(0);
                format!("MultiEdit {v} ({count} edits)")
            })
            .unwrap_or_else(|| "MultiEdit".into()),
        "Write" => input_str(input, "file_path")
            .map(|v| format!("Write {v}"))
            .unwrap_or_else(|| "Write".into()),
        "Bash" => input_str(input, "command")
            .map(|v| format!("Bash {}", truncate(v, 60)))
            .unwrap_or_else(|| "Bash".into()),
        "PowerShell" => input_str(input, "command")
            .map(|v| format!("PowerShell {}", truncate(v, 60)))
            .unwrap_or_else(|| "PowerShell".into()),
        "Agent" => {
            let label = input_str(input, "subagent_type")
                .filter(|agent_type| !agent_type.trim().is_empty())
                .filter(|agent_type| *agent_type != "general-purpose")
                .unwrap_or("Agent");
            input_str(input, "description")
                .or_else(|| input_str(input, "prompt"))
                .map(|v| format!("{label}: {}", truncate(v, 60)))
                .unwrap_or_else(|| label.into())
        }
        "WebFetch" => input_str(input, "url")
            .map(|v| format!("WebFetch {}", truncate(v, 60)))
            .unwrap_or_else(|| "WebFetch".into()),
        "WebSearch" => input_str(input, "query")
            .map(|v| format!("WebSearch \"{}\"", truncate(v, 50)))
            .unwrap_or_else(|| "WebSearch".into()),
        // Every other file tool — NotebookEdit today — labels itself with the
        // path it declares as its target, whatever it calls that field.
        name if rebon_tools_core::file_target_field_for_name(name).is_some() => {
            let field = rebon_tools_core::file_target_field_for_name(name)
                .expect("guard just proved the field is there");
            input_str(input, field)
                .map(|v| format!("{name} {v}"))
                .unwrap_or_else(|| name.into())
        }
        "Skill" => match (input_str(input, "skill"), input_str(input, "args")) {
            (Some(skill), Some(args)) => format!("/{skill} {}", truncate(args, 50)),
            (Some(skill), None) => format!("/{skill}"),
            _ => "Skill".into(),
        },
        "Workflow" | "RunWorkflow" => input_str(input, "name")
            .or_else(|| input_str(input, "title"))
            .or_else(|| input_str(input, "description"))
            .map(|v| format!("Workflow {}", truncate(v, 60)))
            .unwrap_or_else(|| "Workflow".into()),
        "SendMessage" => input_str(input, "to")
            .map(|v| format!("SendMessage to {v}"))
            .unwrap_or_else(|| "SendMessage".into()),
        "TaskCreate" => input_str(input, "subject")
            .map(|v| format!("TaskCreate: {}", truncate(v, 50)))
            .unwrap_or_else(|| "TaskCreate".into()),
        "TaskGet" | "TaskUpdate" | "TaskStop" | "TaskOutput" => input_str(input, "taskId")
            .or_else(|| input_str(input, "task_id"))
            .map(|v| format!("{name} {v}"))
            .unwrap_or_else(|| name.into()),
        "TodoWrite" => "TodoWrite".into(),
        "EnterPlanMode" => "EnterPlanMode".into(),
        "ExitPlanMode" => "ExitPlanMode".into(),
        "EnterWorktree" => input_str(input, "name")
            .map(|v| format!("EnterWorktree {v}"))
            .unwrap_or_else(|| "EnterWorktree".into()),
        "ExitWorktree" => input_str(input, "action")
            .map(|v| format!("ExitWorktree ({v})"))
            .unwrap_or_else(|| "ExitWorktree".into()),
        "ToolSearch" => input_str(input, "query")
            .map(|v| format!("ToolSearch \"{}\"", truncate(v, 50)))
            .unwrap_or_else(|| "ToolSearch".into()),
        _ => name.into(),
    }
}

pub(super) fn tool_name_to_kind(name: &str) -> ToolKind {
    match rebon_tool::render_tool_kind_for_name(name) {
        // Provider-side search tools: not rebon tools, so nothing declares a
        // kind for them, but a transcript still has to draw them.
        ToolKind::Other => match name {
            "web_search" | "web_search_20250305" => ToolKind::Fetch,
            _ => ToolKind::Other,
        },
        kind => kind,
    }
}

/// Construct the `user` transcript entry payload for a batched tool
/// result. The `content` field carries the compacted
/// `ToolResultBlock`s the model receives on the next turn; the
/// `toolUseResults` sibling maps `tool_use_id -> raw JSON` so transcript
/// replay can reconstruct `ToolCallContent::Diff` without a second
/// round-trip through the engine.
///
/// When `outputs` is empty the `toolUseResults` key is elided to keep
/// old-shape transcripts identical on lines where nothing structured
/// survived the tool dispatch (e.g. every call errored).
pub(super) fn tool_result_block_for_transcript(block: &ApiContentBlock) -> ApiContentBlock {
    let ApiContentBlock::ToolResult(tool_result) = block else {
        return block.clone();
    };
    let content = match &tool_result.content {
        ToolResultContent::Text(text) => ToolResultContent::text(text.clone()),
        ToolResultContent::Blocks(blocks) => ToolResultContent::blocks(
            blocks
                .iter()
                .filter_map(|block| match block {
                    ToolResultContentBlock::Text(text) => {
                        Some(ToolResultContentBlock::Text(text.clone()))
                    }
                    ToolResultContentBlock::Image(_) | ToolResultContentBlock::Document(_) => None,
                })
                .collect(),
        ),
    };
    ApiContentBlock::ToolResult(ToolResultBlock {
        tool_use_id: tool_result.tool_use_id.clone(),
        content,
        is_error: tool_result.is_error,
    })
}

pub(super) fn build_tool_result_entry_payload(
    content_value: serde_json::Value,
    outputs: &[(String, serde_json::Value)],
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "message": {
            "role": "user",
            "content": content_value,
        }
    });
    if !outputs.is_empty() {
        let mut map = serde_json::Map::new();
        for (id, value) in outputs {
            map.insert(id.clone(), value.clone());
        }
        payload
            .as_object_mut()
            .unwrap()
            .insert("toolUseResults".to_string(), serde_json::Value::Object(map));
    }
    payload
}

/// Build the batched tool-result entry plus its display-only sidecars.
///
/// Everything past `content_value` lands *beside* `message`, never inside it:
/// `transcript_to_api_messages` reads only `message.content`, so a sidecar is
/// invisible to the model and exists purely so a reloaded transcript can render
/// what the live stream showed.
pub(super) fn build_tool_result_entry_payload_with_sidecars(
    content_value: serde_json::Value,
    outputs: &[(String, serde_json::Value)],
    presentations: &[(String, ToolErrorPresentation)],
    auto_mode_allowed: &[(String, rebon_types::AutoModeAllowSource)],
) -> serde_json::Value {
    let mut payload = build_tool_result_entry_payload(content_value, outputs);
    if !auto_mode_allowed.is_empty() {
        // Entries are objects now that the note names its source. Readers
        // still accept the bare-id strings older transcripts hold — those
        // render the original unattributed note.
        payload.as_object_mut().unwrap().insert(
            "autoModeAllowed".to_string(),
            serde_json::Value::Array(
                auto_mode_allowed
                    .iter()
                    .map(|(id, source)| serde_json::json!({ "id": id, "source": source }))
                    .collect(),
            ),
        );
    }
    if !presentations.is_empty() {
        let mut map = serde_json::Map::new();
        for (id, presentation) in presentations {
            map.insert(
                id.clone(),
                serde_json::json!({
                    "code": presentation.code,
                    "displayMessage": presentation.display_message,
                }),
            );
        }
        payload.as_object_mut().unwrap().insert(
            "toolErrorPresentations".to_string(),
            serde_json::Value::Object(map),
        );
    }
    payload
}

pub(super) fn transcript_content_value(content: &[ApiContentBlock]) -> serde_json::Value {
    if content.len() == 1 {
        match &content[0] {
            ApiContentBlock::Text(text) => serde_json::Value::String(text.text.clone()),
            _ => serde_json::to_value(content).unwrap_or(serde_json::json!([])),
        }
    } else {
        serde_json::to_value(content).unwrap_or(serde_json::json!([]))
    }
}

pub(super) fn user_message_entry_payload(
    content_value: serde_json::Value,
    is_meta: bool,
    runtime_context: bool,
    visible_in_transcript_only: bool,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "message": {
            "role": "user",
            "content": content_value,
        }
    });
    if is_meta {
        payload
            .as_object_mut()
            .unwrap()
            .insert("isMeta".to_string(), serde_json::Value::Bool(true));
    }
    if runtime_context {
        payload
            .as_object_mut()
            .unwrap()
            .insert("runtimeContext".to_string(), serde_json::Value::Bool(true));
    }
    if visible_in_transcript_only {
        payload.as_object_mut().unwrap().insert(
            "isVisibleInTranscriptOnly".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    payload
}

pub(super) fn append_transcript_entry_or_buffer(
    projects_root: &PathBuf,
    cwd: &str,
    session_id: &str,
    entry: TranscriptWriteEntry,
    turn_entries: &mut Vec<TranscriptEntry>,
    log_label: &str,
) {
    match rebon_session::append_transcript_entry(projects_root, cwd, session_id, entry.clone()) {
        Ok(entry) => turn_entries.push(entry),
        Err(err) => {
            tracing::warn!(
                error = %err,
                session_id = %session_id,
                label = log_label,
                "rebon-core failed to persist transcript entry"
            );
            turn_entries.push(rebon_session::finalize_transcript_entry(&entry));
        }
    }
}

pub(super) async fn append_ask_user_question_answer_message(
    projects_root: &PathBuf,
    cwd: &str,
    session_id: &str,
    parent_uuid: &str,
    tool_use_id: &str,
    value: &Value,
    publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    turn_entries: &mut Vec<TranscriptEntry>,
) -> Option<String> {
    let text = format_ask_user_question_answer_for_transcript(value)?;
    let answer_uuid = format!(
        "u-ask-user-question-{session_id}-{tool_use_id}-{}",
        ms_since_epoch()
    );
    let answer_entry = TranscriptWriteEntry::new(
        "user",
        user_message_entry_payload(serde_json::Value::String(text.clone()), false, false, true),
    )
    .with_uuid(answer_uuid.clone())
    .with_parent(parent_uuid.to_string());
    append_transcript_entry_or_buffer(
        projects_root,
        cwd,
        session_id,
        answer_entry,
        turn_entries,
        "ask user question answer",
    );
    publish_session_update(
        publisher,
        session_id,
        SessionUpdate::QueuedUserMessage {
            uuid: answer_uuid.clone(),
            content: vec![AcpContentBlock::Text(TextContent {
                text,
                annotations: None,
            })],
            image_paste_ids: None,
        },
    )
    .await;
    Some(answer_uuid)
}

pub fn transcript_to_api_messages(entries: &[TranscriptEntry]) -> Vec<ApiMessage> {
    let mut out: Vec<ApiMessage> = Vec::new();
    for entry in entries {
        let role = match entry.entry_type.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        if entry
            .raw
            .get("isVisibleInTranscriptOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(content) = extract_content_blocks_from_entry(&entry.raw) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        out.push(ApiMessage { role, content });
    }
    let report = rebon_api::ensure_tool_result_pairing_with_report(&mut out);
    if !report.is_empty() {
        // Emit a single INFO line per resume describing what was
        // repaired so operators can tell a crash-recovery resume apart
        // from a clean one. Persistence of the synthesized results back
        // to the transcript file is a follow-up (needs projects_root
        // threaded through this call path).
        tracing::info!(
            synthesized = report.synthesized.len(),
            globally_orphaned = report.globally_orphaned_result_ids.len(),
            locally_orphaned = report.locally_orphaned_result_ids.len(),
            "transcript_to_api_messages: repaired tool_use/tool_result pairing on resume"
        );
    }
    out
}

pub fn estimate_transcript_input_tokens(entries: &[TranscriptEntry]) -> u32 {
    estimate_messages_input_tokens(None, &transcript_to_api_messages(entries))
}

pub(super) fn replay_prompt_text(requests: &[DenialReplayRequest]) -> String {
    match requests.len() {
        0 => String::new(),
        1 => format!(
            "[Permission replay requested for denial {}: retry {} ({})]",
            requests[0].denial_id, requests[0].tool_use_id, requests[0].tool_name
        ),
        n => format!("[Permission replay requested for {n} denied tool invocation(s)]"),
    }
}

pub(super) fn skill_invocation_prompt_text(invocations: &[SkillInvocationRequest]) -> String {
    match invocations.len() {
        0 => String::new(),
        1 => {
            let args = invocations[0]
                .args
                .as_deref()
                .filter(|args| !args.trim().is_empty())
                .map(|args| format!(" {args}"))
                .unwrap_or_default();
            format!("/{skill}{args}", skill = invocations[0].skill)
        }
        n => format!("[{n} skill invocation(s) requested]"),
    }
}

pub(super) async fn dispatch_skill_invocations(
    engine: &Arc<Engine>,
    context: &ToolContext,
    update_publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    session_id: &str,
    invocations: &[SkillInvocationRequest],
) -> Vec<(ToolUseBlock, ApiContentBlock, Result<Value, String>)> {
    let tools = engine.tool_resolver_for_context(context);
    let mut results = Vec::with_capacity(invocations.len());
    for (index, invocation) in invocations.iter().enumerate() {
        let tool_use_id = format!("toolu-skill-{session_id}-{}-{index}", ms_since_epoch());
        let mut input = serde_json::json!({ "skill": invocation.skill });
        if let Some(args) = invocation
            .args
            .as_deref()
            .filter(|args| !args.trim().is_empty())
        {
            input
                .as_object_mut()
                .unwrap()
                .insert("args".to_string(), Value::String(args.to_string()));
        }
        let tool_use = ToolUseBlock {
            id: tool_use_id.clone(),
            name: rebon_tools_core::SKILL_TOOL_NAME.to_string(),
            input,
        };
        let start_event = QueryEvent::ToolDispatchStart {
            tool_use_id: tool_use_id.clone(),
            name: tool_use.name.clone(),
            input: tool_use.input.clone(),
        };
        forward_tool_dispatch_event(update_publisher, session_id, &start_event).await;
        let (raw_event_tx, mut event_rx) = mpsc::unbounded_channel();
        let event_tx = crate::turn_hook::QueryEventSender::without_hooks(raw_event_tx);
        // No subscribers: this replay path never ran the tool lifecycle
        // events, and this change is not the place to start it.
        let outcome = dispatch_tool_use(
            engine,
            &tool_use,
            context,
            &event_tx,
            crate::policy_seat::PolicySources::default(),
        )
        .await;
        drop(event_tx);
        while let Ok(event) = event_rx.try_recv() {
            forward_tool_dispatch_event(update_publisher, session_id, &event).await;
        }
        let error_presentation = outcome
            .as_ref()
            .err()
            .filter(|error| error.has_distinct_display_message())
            .cloned();
        let public_outcome = outcome.clone().map_err(|error| error.model_message.clone());
        let result_event = QueryEvent::ToolDispatchResult {
            tool_use_id: tool_use_id.clone(),
            name: tool_use.name.clone(),
            outcome: public_outcome.clone(),
            error_presentation,
        };
        forward_tool_dispatch_event(update_publisher, session_id, &result_event).await;
        let (content, is_error) = match &outcome {
            Ok(value) => (
                compact_tool_result_for_model(
                    &tool_use.name,
                    Some(&tool_use.input),
                    value,
                    Some(tools.as_ref()),
                ),
                false,
            ),
            Err(error) => (ToolResultContent::text(error.model_message.clone()), true),
        };
        results.push((
            tool_use,
            ApiContentBlock::ToolResult(ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            }),
            public_outcome,
        ));
    }
    results
}

pub(super) async fn replay_denial_requests(
    engine: &Arc<Engine>,
    context: &ToolContext,
    update_publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    session_id: &str,
    requests: &[DenialReplayRequest],
) -> Result<Vec<(String, String, ApiContentBlock, Result<Value, String>)>, PromptExecutorError> {
    // The per-request replay context only adds a tool_use id and a denial
    // reason, neither of which changes which tools resolve, so one resolver
    // serves the whole batch.
    let tools = engine.tool_resolver_for_context(context);
    let mut results = Vec::with_capacity(requests.len());
    for request in requests {
        let input: Value = serde_json::from_str(&request.tool_input).map_err(|err| {
            PromptExecutorError::Execution(format!(
                "permission replay {} has invalid tool_input JSON: {err}",
                request.denial_id
            ))
        })?;
        let replay_context = context
            .with_tool_use_id(request.tool_use_id.clone())
            .with_denial_replay_reason(request.reason.clone());
        let event = QueryEvent::ToolDispatchStart {
            tool_use_id: request.tool_use_id.clone(),
            name: request.tool_name.clone(),
            input: input.clone(),
        };
        forward_tool_dispatch_event(update_publisher, session_id, &event).await;
        let (_progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let (raw_event_tx, mut event_rx) = mpsc::unbounded_channel();
        let event_tx = crate::turn_hook::QueryEventSender::without_hooks(raw_event_tx);
        let tool_use = ToolUseBlock {
            id: request.tool_use_id.clone(),
            name: request.tool_name.clone(),
            input,
        };
        // As above: replaying a recorded call asks nobody.
        let outcome = dispatch_tool_use(
            engine,
            &tool_use,
            &replay_context,
            &event_tx,
            crate::policy_seat::PolicySources::default(),
        )
        .await;
        drop(event_tx);
        while let Ok(event) = event_rx.try_recv() {
            forward_tool_dispatch_event(update_publisher, session_id, &event).await;
        }
        while let Ok(progress) = progress_rx.try_recv() {
            let progress_event = QueryEvent::ToolDispatchProgress {
                tool_use_id: request.tool_use_id.clone(),
                name: request.tool_name.clone(),
                progress,
            };
            forward_tool_dispatch_event(update_publisher, session_id, &progress_event).await;
        }
        let error_presentation = outcome
            .as_ref()
            .err()
            .filter(|error| error.has_distinct_display_message())
            .cloned();
        let public_outcome = outcome.clone().map_err(|error| error.model_message.clone());
        let result_event = QueryEvent::ToolDispatchResult {
            tool_use_id: request.tool_use_id.clone(),
            name: request.tool_name.clone(),
            outcome: public_outcome.clone(),
            error_presentation,
        };
        forward_tool_dispatch_event(update_publisher, session_id, &result_event).await;
        let permission_extra_text = outcome
            .as_ref()
            .ok()
            .and_then(|value| value.get("permissionExtraText"))
            .and_then(Value::as_str);
        let (mut content, is_error) = match &outcome {
            Ok(value) => (
                compact_tool_result_for_model(
                    &request.tool_name,
                    Some(&tool_use.input),
                    value,
                    Some(tools.as_ref()),
                ),
                false,
            ),
            Err(error) => (ToolResultContent::text(error.model_message.clone()), true),
        };
        append_permission_extra_text_to_tool_result(&mut content, permission_extra_text);
        results.push((
            request.tool_use_id.clone(),
            request.tool_name.clone(),
            ApiContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: request.tool_use_id.clone(),
                content,
                is_error,
            }),
            public_outcome,
        ));
    }
    Ok(results)
}

/// Pull the `message.content` payload of a transcript entry into
/// a list of [`ApiContentBlock`]s. Returns `None` when the entry
/// has no recognisable content.
pub(super) fn extract_content_blocks_from_entry(
    raw: &serde_json::Value,
) -> Option<Vec<ApiContentBlock>> {
    if let Some(message) = raw.get("message") {
        if let Some(content) = message.get("content") {
            let queued_command = raw
                .get("queuedCommand")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let content = if queued_command {
                raw.get("modelContent").unwrap_or(content)
            } else {
                content
            };
            let mut blocks = content_value_to_blocks(content)?;
            if queued_command {
                wrap_queued_user_content_blocks(&mut blocks);
            }
            return Some(blocks);
        }
    }
    // Legacy fallback: a top-level `text` field with no structured
    // shape. Useful for hand-authored fixtures.
    raw.get("text").and_then(|v| v.as_str()).map(|s| {
        vec![ApiContentBlock::Text(TextBlock {
            text: s.to_string(),
        })]
    })
}

pub(super) fn content_value_to_blocks(value: &serde_json::Value) -> Option<Vec<ApiContentBlock>> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => {
            Some(vec![ApiContentBlock::Text(TextBlock { text: s.clone() })])
        }
        serde_json::Value::String(_) => Some(Vec::new()),
        serde_json::Value::Array(items) => {
            let mut out: Vec<ApiContentBlock> = Vec::new();
            for item in items {
                if let Some(block) = content_block_from_value(item) {
                    out.push(block);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

pub(super) fn content_block_from_value(item: &serde_json::Value) -> Option<ApiContentBlock> {
    let kind = item.get("type").and_then(|v| v.as_str())?;
    match kind {
        "text" => {
            let text = item
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(ApiContentBlock::Text(TextBlock { text }))
        }
        "image" => parse_image_content_block(item),
        "tool_use" => {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = item
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));
            if id.is_empty() || name.is_empty() {
                return None;
            }
            Some(ApiContentBlock::ToolUse(ToolUseBlock { id, name, input }))
        }
        "tool_result" => {
            let tool_use_id = item
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if tool_use_id.is_empty() {
                return None;
            }
            // `content` can be a plain string or an array of
            // typed blocks; the API layer stores it as a string,
            // so we stringify arrays to preserve structure.
            //
            // Cap at MAX_TOOL_RESULT_CHARS — old JSONL transcripts
            // may contain full file bodies (e.g. Edit returning the
            // entire updated file) that were never compacted.
            let content = match item.get("content") {
                Some(serde_json::Value::String(s)) => {
                    ToolResultContent::text(truncate_tool_result_content(s.clone()))
                }
                Some(serde_json::Value::Array(items)) => {
                    ToolResultContent::blocks(parse_tool_result_content_blocks(items))
                }
                Some(other) => {
                    ToolResultContent::text(truncate_tool_result_content(other.to_string()))
                }
                None => ToolResultContent::text(String::new()),
            };
            let is_error = item
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(ApiContentBlock::ToolResult(ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            }))
        }
        "thinking" => {
            // Preserve reasoning across resume so it round-trips exactly
            // like the live controller (TurnControlPlugin re-pushes full assistant
            // content, thinking included). Providers that REQUIRE it back
            // — DeepSeek's ReasoningReplayMode::Always 400s without it —
            // need it present; deciding what to actually send is the wire
            // layer's job (anthropic `message_to_wire` drops signatureless
            // blocks, `strip_old_thinking_blocks` trims old turns), not the
            // transcript loader's. The persisted `signature` is preserved.
            let thinking = item
                .get("thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let signature = item
                .get("signature")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(ApiContentBlock::Thinking(rebon_api::ThinkingBlock {
                thinking,
                signature,
                data: None,
            }))
        }
        "redacted_thinking" => {
            // Redacted thinking carries its (encrypted) reasoning in
            // `data`; preserve it so it round-trips as a valid
            // `redacted_thinking` block instead of being dropped.
            let data = item
                .get("data")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(ApiContentBlock::Thinking(rebon_api::ThinkingBlock {
                thinking: String::new(),
                signature: None,
                data,
            }))
        }
        "compaction" => {
            // Server-side compaction stands in for the history it
            // replaced, so unlike the other server-side blocks it MUST
            // survive resume — drop it and the session comes back with
            // the summary's *inputs* gone and nothing in their place.
            // `content` is Anthropic's plaintext summary,
            // `encrypted_content` OpenAI's opaque blob; a block with
            // neither is a husk and carries nothing to replay.
            let content = item
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let encrypted_content = item
                .get("encrypted_content")
                .and_then(|v| v.as_str())
                .filter(|content| !content.is_empty())
                .map(str::to_string);
            if content.is_none() && encrypted_content.is_none() {
                return None;
            }
            Some(ApiContentBlock::Compaction(rebon_api::CompactionBlock {
                content,
                encrypted_content,
            }))
        }
        "server_tool_use" | "web_search_result" => {
            // Server-side tool uses and their results are handled
            // inline by the provider — they don't need to be
            // replayed to the model on subsequent turns.
            None
        }
        _ => None,
    }
}

pub(super) fn parse_image_content_block(item: &serde_json::Value) -> Option<ApiContentBlock> {
    let source = item.get("source")?;
    let kind = source
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("base64");
    if kind != "base64" {
        return None;
    }
    let media_type = source
        .get("media_type")
        .or_else(|| source.get("mediaType"))
        .and_then(|v| v.as_str())?;
    let data = source.get("data").and_then(|v| v.as_str())?;
    Some(ApiContentBlock::Image(ImageBlock::base64(media_type, data)))
}

pub(super) fn parse_tool_result_content_blocks(
    items: &[serde_json::Value],
) -> Vec<ToolResultContentBlock> {
    items
        .iter()
        .filter_map(|item| match item.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                let text = item
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(ToolResultContentBlock::Text(TextBlock { text }))
            }
            Some("image") => match parse_image_content_block(item) {
                Some(ApiContentBlock::Image(image)) => Some(ToolResultContentBlock::Image(image)),
                _ => None,
            },
            Some("document") => {
                let source = item.get("source")?;
                let kind = source
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("base64");
                if kind != "base64" {
                    return None;
                }
                let media_type = source
                    .get("media_type")
                    .or_else(|| source.get("mediaType"))
                    .and_then(|v| v.as_str())?;
                let data = source.get("data").and_then(|v| v.as_str())?;
                Some(ToolResultContentBlock::Document(DocumentBlock::base64(
                    media_type, data,
                )))
            }
            _ => None,
        })
        .collect()
}

pub(super) const LOCAL_QUEUED_USER_MARKER_PREFIX: &str = "<rebon-queued-user-input uuid=\"";
pub(super) const LOCAL_QUEUED_MODEL_TEXT_MARKER_PREFIX: &str = "<rebon-queued-user-model-text>\n";
const VISIBLE_RUNTIME_ATTACHMENT_MARKER_PREFIX: &str = "<rebon-visible-runtime-attachment uuid=\"";
const VISIBLE_RUNTIME_MODEL_TEXT_MARKER_PREFIX: &str = "<rebon-visible-runtime-model-text>\n";

pub fn visible_runtime_attachment_message(
    uuid: &str,
    visible_text: String,
    model_text: String,
) -> ApiMessage {
    ApiMessage {
        role: Role::User,
        content: vec![
            ApiContentBlock::Text(TextBlock {
                text: format!("{VISIBLE_RUNTIME_ATTACHMENT_MARKER_PREFIX}{uuid}\">"),
            }),
            ApiContentBlock::Text(TextBlock { text: visible_text }),
            ApiContentBlock::Text(TextBlock {
                text: format!("{VISIBLE_RUNTIME_MODEL_TEXT_MARKER_PREFIX}{model_text}"),
            }),
        ],
    }
}

fn attachment_uuid_from_marker(text: &str, prefix: &str) -> Option<String> {
    text.strip_prefix(prefix)
        .and_then(|rest| rest.split_once('"'))
        .map(|(uuid, _)| uuid.to_string())
}

pub(super) fn local_user_uuid_from_attachment(message: &ApiMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ApiContentBlock::Text(text) => {
            attachment_uuid_from_marker(&text.text, LOCAL_QUEUED_USER_MARKER_PREFIX).or_else(|| {
                attachment_uuid_from_marker(&text.text, VISIBLE_RUNTIME_ATTACHMENT_MARKER_PREFIX)
            })
        }
        _ => None,
    })
}

pub(super) fn local_image_paste_ids_from_attachment(message: &ApiMessage) -> Vec<u32> {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ApiContentBlock::Text(text) => {
                let marker = text.text.strip_prefix(LOCAL_QUEUED_USER_MARKER_PREFIX)?;
                let (_, rest) = marker.split_once('"')?;
                let ids = rest.split_once(" imagePasteIds=\"")?.1.split_once('"')?.0;
                Some(
                    ids.split(',')
                        .filter_map(|id| id.parse::<u32>().ok())
                        .collect::<Vec<_>>(),
                )
            }
            _ => None,
        })
        .unwrap_or_default()
}

pub(super) fn local_queued_model_text_from_attachment(message: &ApiMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ApiContentBlock::Text(text) => text
            .text
            .strip_prefix(LOCAL_QUEUED_MODEL_TEXT_MARKER_PREFIX)
            .or_else(|| {
                text.text
                    .strip_prefix(VISIBLE_RUNTIME_MODEL_TEXT_MARKER_PREFIX)
            })
            .map(str::to_string),
        _ => None,
    })
}

pub(super) fn strip_local_user_uuid_marker(message: &ApiMessage) -> ApiMessage {
    let mut message = message.clone();
    message.content.retain(|block| match block {
        ApiContentBlock::Text(text) => {
            !text.text.starts_with(LOCAL_QUEUED_USER_MARKER_PREFIX)
                && !text.text.starts_with(LOCAL_QUEUED_MODEL_TEXT_MARKER_PREFIX)
                && !text
                    .text
                    .starts_with(VISIBLE_RUNTIME_ATTACHMENT_MARKER_PREFIX)
                && !text
                    .text
                    .starts_with(VISIBLE_RUNTIME_MODEL_TEXT_MARKER_PREFIX)
        }
        _ => true,
    });
    message
}

pub(super) fn visible_message_for_attachment(message: &ApiMessage) -> ApiMessage {
    strip_local_user_uuid_marker(message)
}

pub(super) fn model_visible_message_for_attachment(message: &ApiMessage) -> ApiMessage {
    if let Some(model_text) = local_queued_model_text_from_attachment(message) {
        let mut model_message = ApiMessage {
            role: message.role.clone(),
            content: vec![ApiContentBlock::Text(TextBlock { text: model_text })],
        };
        model_message.content.extend(
            message
                .content
                .iter()
                .filter(|block| matches!(block, ApiContentBlock::Image(_)))
                .cloned(),
        );
        return model_message;
    }

    strip_local_user_uuid_marker(message)
}

pub(super) fn wrap_queued_user_text(raw: &str) -> String {
    format!(
        "<system-reminder>\nThe user sent a new message while you were working:\n{raw}\n\nIMPORTANT: After completing your current task, you MUST address the user's message above. Do not ignore it.\n</system-reminder>"
    )
}

pub(super) fn wrap_queued_user_content_blocks(blocks: &mut Vec<ApiContentBlock>) {
    let mut wrapped_text = false;
    for block in blocks.iter_mut() {
        if let ApiContentBlock::Text(text) = block {
            text.text = wrap_queued_user_text(&text.text);
            wrapped_text = true;
        }
    }
    if !wrapped_text {
        blocks.insert(
            0,
            ApiContentBlock::Text(TextBlock {
                text: wrap_queued_user_text(""),
            }),
        );
    }
}

pub(crate) fn model_message_for_attachment(message: &ApiMessage) -> ApiMessage {
    let is_local_queued_user = message.content.iter().any(|block| {
        matches!(
            block,
            ApiContentBlock::Text(text)
                if text.text.starts_with(LOCAL_QUEUED_USER_MARKER_PREFIX)
        )
    });
    let mut message = model_visible_message_for_attachment(message);
    if is_local_queued_user {
        wrap_queued_user_content_blocks(&mut message.content);
    }
    message
}

pub(super) fn api_blocks_to_acp_content_blocks(blocks: &[ApiContentBlock]) -> Vec<AcpContentBlock> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ApiContentBlock::Text(text) => Some(AcpContentBlock::Text(TextContent {
                text: text.text.clone(),
                annotations: None,
            })),
            ApiContentBlock::Image(image) => {
                Some(AcpContentBlock::Image(rebon_types::ImageContent {
                    mime_type: image.source.media_type.clone(),
                    data: image.source.data.clone(),
                    uri: None,
                    annotations: None,
                }))
            }
            _ => None,
        })
        .collect()
}

pub(super) fn acp_blocks_to_api_content_blocks(blocks: &[AcpContentBlock]) -> Vec<ApiContentBlock> {
    blocks
        .iter()
        .filter_map(|block| match block {
            AcpContentBlock::Text(t) => Some(ApiContentBlock::Text(TextBlock {
                text: t.text.clone(),
            })),
            AcpContentBlock::Image(image) => Some(ApiContentBlock::Image(ImageBlock::base64(
                image.mime_type.clone(),
                image.data.clone(),
            ))),
            _ => None,
        })
        .collect()
}

pub(super) fn api_content_has_model_visible_content(blocks: &[ApiContentBlock]) -> bool {
    blocks.iter().any(|block| match block {
        ApiContentBlock::Text(text) => !text.text.trim().is_empty(),
        _ => true,
    })
}

pub(super) fn acp_blocks_to_text(blocks: &[AcpContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            AcpContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
