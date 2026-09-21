use super::*;

pub(super) struct SessionTitleGenerationRequest {
    pub(super) session: Arc<SessionHandle>,
    pub(super) title_model: String,
    pub(super) projects_root: PathBuf,
    pub(super) cwd: String,
    pub(super) session_id: String,
    pub(super) update_publisher: Option<Arc<dyn SessionUpdatePublisher>>,
    pub(super) server_state: Option<Arc<ServerState>>,
    pub(super) existing_title: Option<String>,
    /// `Some` is the exact bounded extraction from the complete normalized
    /// prior transcript. `None` preserves the legacy <3-message fallback to
    /// the current user prompt.
    pub(super) prior_conversation_text: Option<String>,
    pub(super) user_text: String,
}

pub(super) fn maybe_spawn_session_title_generation(request: SessionTitleGenerationRequest) {
    if request
        .existing_title
        .as_deref()
        .is_some_and(|title| !title.trim().is_empty())
    {
        return;
    }
    if is_synthetic_title_source(&request.user_text) {
        return;
    }

    let conversation_text = request
        .prior_conversation_text
        .unwrap_or_else(|| request.user_text.clone());
    if conversation_text.trim().is_empty() {
        return;
    }

    if request.title_model.trim().is_empty() {
        tracing::debug!(
            session_id = %request.session_id,
            "session-title: skipping generation because no title model is configured"
        );
        return;
    }

    // Title generation runs alongside the user's turn on its own
    // forked session, so it neither pollutes the continuation chain
    // nor signals turn end on the session the user is still using.
    let title_session = request.session.fork_for_sub_agent(None);

    tokio::spawn(async move {
        let Some(title) = rebon_api::generate_session_title(
            title_session.client().as_ref(),
            &request.title_model,
            &conversation_text,
        )
        .await
        else {
            return;
        };

        if let Err(err) = rebon_session::save_session_title(
            &request.projects_root,
            &request.cwd,
            &request.session_id,
            &title,
        ) {
            tracing::debug!(
                error = %err,
                session_id = %request.session_id,
                "session-title: failed to persist generated title"
            );
        }
        if let Some(state) = request.server_state.as_ref() {
            state.set_session_title(&request.session_id, title.clone());
        }
        if let Some(publisher) = request.update_publisher.as_ref() {
            publisher
                .publish_to(
                    &request.session_id,
                    SessionUpdate::SessionInfoUpdate {
                        title: Some(title),
                        updated_at: Some(rebon_session::format_system_time_iso_ms(
                            SystemTime::now(),
                        )),
                        meta: None,
                    },
                )
                .await;
        }
    });
}

pub(super) fn is_synthetic_title_source(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<local-command-stdout>")
        || text.starts_with("<command-message>")
        || text.starts_with("<command-name>")
        || text.starts_with("<bash-input>")
}

pub(crate) fn stable_base_system_enabled() -> bool {
    !rebon_types::env::env_defined_falsy("REBON_STABLE_BASE_SYSTEM")
}

pub(super) fn virtual_runtime_context_message(runtime_context: &str) -> ApiMessage {
    rebon_api::runtime_context_message(runtime_context)
}

pub(super) fn latest_durable_runtime_context_body(messages: &[ApiMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find_map(rebon_api::runtime_context_body_from_message)
}

pub(super) fn current_plain_user_tail_index(messages: &[ApiMessage]) -> Option<usize> {
    let idx = messages.len().checked_sub(1)?;
    let message = messages.get(idx)?;
    if message.role == Role::User
        && message
            .content
            .iter()
            .all(|block| !matches!(block, ApiContentBlock::ToolResult(_)))
    {
        Some(idx)
    } else {
        None
    }
}

pub(super) fn min_tail_messages_preserving_runtime_context(
    messages: &[ApiMessage],
    default_min_tail: usize,
) -> usize {
    if messages.len() >= 2
        && rebon_api::is_runtime_context_message(&messages[messages.len() - 2])
        && current_plain_user_tail_index(messages).is_some()
    {
        default_min_tail.max(2)
    } else {
        default_min_tail
    }
}

pub(super) fn materialize_durable_transient_context(
    messages: &mut Vec<ApiMessage>,
    transient_context_message: &mut Option<String>,
) -> Option<ApiMessage> {
    let Some(context) = transient_context_message.as_deref() else {
        return None;
    };
    if context.is_empty() {
        *transient_context_message = None;
        return None;
    }
    if latest_durable_runtime_context_body(messages) == Some(context) {
        *transient_context_message = None;
        return None;
    }

    // Durable transient context is a separate user-role reminder that must
    // precede the current user prompt. Callers are expected to append the
    // current plain user prompt before this helper runs. If that contract is
    // violated, preserve the request-scoped transient context instead of
    // appending a durable reminder after the wrong turn.
    let Some(insert_at) = current_plain_user_tail_index(messages) else {
        tracing::warn!(
            message_count = messages.len(),
            "durable transient context requires a current plain user tail; preserving request-scoped transient context"
        );
        return None;
    };

    let context = transient_context_message
        .take()
        .expect("transient context checked above");
    let message = virtual_runtime_context_message(&context);
    messages.insert(insert_at, message.clone());
    Some(message)
}

pub(super) fn messages_with_runtime_context(
    mut request_messages: Vec<ApiMessage>,
    runtime_context_message: Option<&str>,
) -> Vec<ApiMessage> {
    if let Some(runtime_context) = runtime_context_message {
        if !runtime_context.is_empty() {
            request_messages.insert(0, virtual_runtime_context_message(runtime_context));
        }
    }
    request_messages
}

pub(super) fn request_messages_with_runtime_context(
    manager: &ContextManager,
    runtime_context_message: Option<&str>,
) -> Vec<ApiMessage> {
    messages_with_runtime_context(manager.messages_for_request(), runtime_context_message)
}

pub(super) fn build_model_request(
    manager: &ContextManager,
    params: &QueryParams,
) -> CreateMessageRequest {
    let request_messages =
        request_messages_with_runtime_context(manager, params.runtime_context_message.as_deref());
    CreateMessageRequest {
        model: params.model.clone(),
        messages: request_messages,
        system: params.system.clone(),
        transient_context: params.transient_context_message.clone(),
        tools: params.tools.clone(),
        tool_choice: params.next_tool_choice.clone(),
        max_tokens: params.max_tokens,
        temperature: None,
        stop_sequences: Vec::new(),
        stream: true,
        metadata: None,
        thinking: params.thinking.clone(),
        reasoning_effort: params.reasoning_effort,
        reasoning_mode: params.reasoning_mode,
        reasoning_summary: None,
        web_search: params.web_search.clone(),
        context_management: params.context_management.clone(),
        cache_trace_context: params.cache_trace_context.clone(),
        compaction_trigger: false,
    }
}

pub(super) fn estimate_request_input_tokens(request: &CreateMessageRequest) -> u32 {
    let effective_messages = request.messages_with_transient_context();
    let message_tokens =
        estimate_messages_input_tokens(request.system.as_deref(), &effective_messages);
    let tool_tokens =
        prompt_tool_metadata_report("provider-visible tools", &request.tools).estimated_tokens;
    message_tokens.saturating_add(tool_tokens.min(u32::MAX as u64) as u32)
}

pub(super) fn full_request_input_estimate(manager: &ContextManager, params: &QueryParams) -> u32 {
    estimate_request_input_tokens(&build_model_request(manager, params))
}

pub(super) fn request_dynamic_overhead_tokens(
    manager: &ContextManager,
    params: &QueryParams,
) -> u32 {
    full_request_input_estimate(manager, params).saturating_sub(manager.estimate_input_tokens())
}

pub(super) fn next_request_input_estimate(manager: &ContextManager, params: &QueryParams) -> u32 {
    // The server baseline captures provider-only costs such as encrypted
    // reasoning that local message estimation cannot reproduce.
    full_request_input_estimate(manager, params).max(manager.next_request_estimate())
}

pub(super) fn history_target_for_request_budget(
    manager: &ContextManager,
    params: &QueryParams,
    request_target_tokens: u32,
) -> u32 {
    let overhead = request_dynamic_overhead_tokens(manager, params);
    request_target_tokens.saturating_sub(overhead).max(1)
}

pub(super) fn report_estimated_usage_after_compact(manager: &ContextManager, params: &QueryParams) {
    if let Some(handle) = &params.prune_level {
        let estimated = next_request_input_estimate(manager, params);
        handle.report_estimated_usage(estimated);
        tracing::debug!(
            estimated_input_tokens = estimated,
            "context manager: recomputed estimated request usage"
        );
    }
}

pub fn build_request_prompt_cost_report(request: &CreateMessageRequest) -> PromptCostReport {
    let mut sections = Vec::new();
    if let Some(system) = request.system.as_deref().filter(|s| !s.is_empty()) {
        sections.push(prompt_text_section_report(
            "provider system prompt",
            "provider_system",
            system,
        ));
    }
    if let Some(transient) = request
        .transient_context
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        sections.push(prompt_text_section_report(
            "transient context",
            "provider_transient_context",
            transient,
        ));
    }
    sections.push(prompt_tool_metadata_report(
        "provider-visible tools",
        &request.tools,
    ));
    sections.push(prompt_messages_report(
        "message/history blocks",
        &request.messages,
    ));
    PromptCostReport::new(sections)
}

pub(super) fn ms_since_epoch() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(super) fn anchored_minimal_content_has_anchor(
    role: &Role,
    content: &[ApiContentBlock],
) -> bool {
    content.iter().any(|block| match block {
        ApiContentBlock::ToolResult(_) => true,
        ApiContentBlock::Text(text) if *role == Role::Assistant => !text.text.trim().is_empty(),
        ApiContentBlock::ToolUse(_) | ApiContentBlock::ServerToolUse(_)
            if *role == Role::Assistant =>
        {
            true
        }
        ApiContentBlock::GeneratedImage(_) if *role == Role::Assistant => true,
        _ => false,
    })
}

pub(super) fn history_has_anchored_minimal_anchor(history: &[ApiMessage]) -> bool {
    history
        .iter()
        .any(|message| anchored_minimal_content_has_anchor(&message.role, &message.content))
}

/// Whether an Anchored-Minimal bootstrap turn may clamp its output budget.
///
/// The clamp keeps the first exploratory reply cheap, but it can only ever be
/// spent once. Where the provider's output budget covers hidden reasoning, the
/// ceiling is consumed by the chain of thought before any visible block, and the
/// resulting `Thinking`-only reply is deliberately not an anchor — so the next
/// turn would bootstrap again, identically, forever. The same is true of any
/// other reason a bootstrap reply lands without an anchor, hence the second
/// guard. The reduced bootstrap tool surface is unaffected either way.
pub(super) fn anchored_minimal_budget_clamp_applies(
    bootstrap: bool,
    output_budget_includes_reasoning: bool,
    history_has_assistant_turn: bool,
) -> bool {
    bootstrap && !output_budget_includes_reasoning && !history_has_assistant_turn
}

/// Whether the model has ever replied in this session, anchor or not.
///
/// A `Thinking`-only reply is deliberately not an anchor, so a bootstrap turn
/// that spends its whole output budget on reasoning leaves the session in the
/// bootstrap phase forever. This is the escape hatch: the phase still starts at
/// bootstrap, but the clamped budget that caused the empty reply is only
/// applied while no assistant turn exists at all.
pub(super) fn history_has_assistant_turn(history: &[ApiMessage]) -> bool {
    history
        .iter()
        .any(|message| message.role == Role::Assistant)
}

pub(super) fn apply_anchored_minimal_promotion(
    params: &mut QueryParams,
    context: &mut ToolContext,
) -> bool {
    let Some(promotion) = params.anchored_minimal_promotion.take() else {
        return false;
    };

    params.tools = promotion.tools;
    params.runtime_context_message = promotion.runtime_context_message;
    params.transient_context_message = promotion.transient_context_message;
    params.attachment_poller = promotion.attachment_poller;
    params.max_tokens = promotion.max_tokens;
    params.capability_mode = AgentCapabilityMode::Normal;

    let discovered_tools = context.discovered_deferred_tool_names();
    let mut next_context = context.clone().without_tool_search_index();
    if !promotion.tool_search_index.is_empty() {
        next_context = next_context.with_tool_search_index(promotion.tool_search_index);
        for name in discovered_tools {
            next_context.record_discovered_deferred_tool(&name);
        }
    }
    *context = next_context;
    true
}

pub(super) fn release_request_policy_after_context_reset(
    engine: &Engine,
    params: &mut QueryParams,
    context: &mut ToolContext,
) {
    params.execution_policy = None;
    params.effective_tool_filter = params.base_tool_filter.clone();
    if let Some(system) = params.post_context_reset_system.clone() {
        params.system = Some(system);
    }
    params.runtime_context_message = params.post_context_reset_runtime_context_message.clone();
    params.transient_context_message = params.post_context_reset_transient_context_message.clone();

    let projection = runtime_tool_projection_for_mode(
        engine,
        rebon_tool::is_tool_search_enabled(),
        params.effective_tool_filter.as_ref(),
        params.invariant_execution_policy.as_ref(),
        &[],
        &params.mcp_tool_definitions,
        params.capability_mode,
        context.plugin_tools(),
    );
    let tool_search_index = projection.tool_search_index.clone();
    let mcp_tool_definitions = Arc::new(params.mcp_tool_definitions.clone());
    params.tools = projection.provider_visible_tools;

    let mut next_context = context
        .clone()
        .with_optional_execution_policy(params.invariant_execution_policy.clone())
        .without_tool_search_index()
        .with_mcp_tool_definitions(mcp_tool_definitions)
        .with_tool_filter(params.effective_tool_filter.clone());
    if !tool_search_index.is_empty() {
        next_context = next_context.with_tool_search_index(tool_search_index);
    }
    *context = next_context;
}
