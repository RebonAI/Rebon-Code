use serde_json::{json, Value};

use crate::request::CreateMessageRequest;
use crate::types::{
    ContentBlock, ImageBlock, Message, Role, Tool, ToolResultContent, ToolResultContentBlock,
};
use crate::ServiceTierHandle;

// ---------------------------------------------------------------------------
// Request body translation
// ---------------------------------------------------------------------------

/// Translate a provider-agnostic [`CreateMessageRequest`] into a
/// Responses API `POST /responses` JSON body.
///
/// `is_codex` drives the stricter-params rule that applies to the
/// ChatGPT Codex backend: that endpoint rejects
/// `max_output_tokens`, `temperature`, `top_p`, and `tool_choice`,
/// and requires `store: false`. Non-Codex callers flip the flag
/// and those fields are emitted normally.
///
/// `prompt_cache_key` is required on every request to let the
/// backend share prompt-cache hits across turns of the same
/// session — [`OpenAiResponsesProvider::new`] generates a stable
/// per-process value if the caller does not supply one.
pub fn build_responses_request_body(
    request: &CreateMessageRequest,
    is_codex: bool,
    prompt_cache_key: &str,
    previous_response_id: Option<&str>,
) -> Value {
    build_responses_request_body_with_service_tier(
        request,
        is_codex,
        prompt_cache_key,
        previous_response_id,
        None,
    )
}

pub fn build_responses_request_body_with_service_tier(
    request: &CreateMessageRequest,
    is_codex: bool,
    prompt_cache_key: &str,
    previous_response_id: Option<&str>,
    service_tier: Option<&ServiceTierHandle>,
) -> Value {
    let input = build_responses_input(&request.messages_with_transient_context());
    build_responses_request_body_with_input_and_service_tier(
        request,
        input,
        is_codex,
        prompt_cache_key,
        previous_response_id,
        service_tier,
    )
}

pub(super) fn build_responses_request_body_with_input_and_service_tier(
    request: &CreateMessageRequest,
    input: Vec<Value>,
    is_codex: bool,
    prompt_cache_key: &str,
    previous_response_id: Option<&str>,
    service_tier: Option<&ServiceTierHandle>,
) -> Value {
    let instructions = request
        .system
        .clone()
        .unwrap_or_else(|| "You are a helpful assistant.".to_string());

    let mut tools = build_responses_tools(&request.tools);

    // Virtual "-pro" aliases (gpt-5.6 family): rebon models them as
    // standalone entries, but the wire request is the base slug plus
    // `reasoning.mode: "pro"`.
    let (wire_model, alias_reasoning_mode) =
        match crate::request::split_pro_model_alias(&request.model) {
            Some(base) => (base, Some(crate::request::ReasoningMode::Pro)),
            None => (request.model.as_str(), None),
        };

    // Remote compaction v2 is a normal turn with one extra control
    // item at the tail: the whole point is that everything before it is
    // the live prefix, byte-identical to the session's own requests, so
    // the backend bills the replayed history at the cached rate.
    let input = if request.compaction_trigger {
        let mut input = input;
        input.push(json!({ "type": "compaction_trigger" }));
        input
    } else {
        input
    };

    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(wire_model));
    body.insert("input".into(), Value::Array(input));
    body.insert("instructions".into(), json!(instructions));
    body.insert("store".into(), json!(!is_codex));
    body.insert("prompt_cache_key".into(), json!(prompt_cache_key));

    // When present the server can skip re-processing prior input
    // items, saving tokens and latency.
    if let Some(prev_id) = previous_response_id {
        body.insert("previous_response_id".into(), json!(prev_id));
    }

    // Codex backend disables these optional params entirely.
    if !is_codex {
        body.insert("max_output_tokens".into(), json!(request.max_tokens));
        if let Some(temp) = request.temperature {
            body.insert("temperature".into(), json!(temp));
        }
    }

    // Inject the native web search tool when configured.
    if let Some(ws) = &request.web_search {
        let mut ws_tool = json!({
            "type": "web_search",
        });
        if let Some(ref domains) = ws.allowed_domains {
            ws_tool["filters"] = json!({ "allowed_domains": domains });
        }
        if let Some(ref size) = ws.search_context_size {
            ws_tool["search_context_size"] = json!(size);
        }
        if let Some(ref loc) = ws.user_location {
            let mut loc_obj = json!({ "type": "approximate" });
            if let Some(ref c) = loc.country {
                loc_obj["country"] = json!(c);
            }
            if let Some(ref r) = loc.region {
                loc_obj["region"] = json!(r);
            }
            if let Some(ref c) = loc.city {
                loc_obj["city"] = json!(c);
            }
            if let Some(ref t) = loc.timezone {
                loc_obj["timezone"] = json!(t);
            }
            ws_tool["user_location"] = loc_obj;
        }
        tools.push(ws_tool);
    }

    if !tools.is_empty() {
        body.insert("tools".into(), json!(tools));
    }

    // Reasoning / thinking: when the request carries thinking config
    // or explicit reasoning params, emit the `reasoning` field and
    // `include` array. Matches codex-ref's `build_responses_request()`
    // which conditionally includes `reasoning: { effort, summary }`
    // and `include: ["reasoning.encrypted_content"]`.
    {
        use crate::request::{ReasoningEffort, ReasoningSummary, ThinkingConfig};
        let has_thinking = matches!(request.thinking, Some(ThinkingConfig::Enabled { .. }));
        let effort = request.reasoning_effort.or_else(|| {
            if has_thinking {
                Some(ReasoningEffort::High)
            } else {
                None
            }
        });
        let reasoning_mode = request.reasoning_mode.or(alias_reasoning_mode);
        if effort.is_some() || request.reasoning_summary.is_some() || reasoning_mode.is_some() {
            let mut reasoning = serde_json::Map::new();
            if let Some(e) = effort {
                reasoning.insert(
                    "effort".into(),
                    serde_json::to_value(e).unwrap_or(json!("high")),
                );
            }
            // Pro mode (gpt-5.6+): `reasoning.mode: "pro"` replaces a
            // separate Pro model slug. Omitted = standard mode.
            if let Some(mode) = reasoning_mode {
                reasoning.insert(
                    "mode".into(),
                    serde_json::to_value(mode).unwrap_or(json!("pro")),
                );
            }
            // Default summary to "auto" when reasoning is enabled
            // but no explicit summary was set. Without this the API
            // won't return `response.reasoning_summary_text.delta`
            // events.
            let summary = request.reasoning_summary.unwrap_or(ReasoningSummary::Auto);
            reasoning.insert(
                "summary".into(),
                serde_json::to_value(summary).unwrap_or(json!("auto")),
            );
            body.insert("reasoning".into(), Value::Object(reasoning));

            // Tell the API to include reasoning content in the
            // response stream. Without this, reasoning events are
            // suppressed.
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }

    body.insert("stream".into(), json!(true));
    let mut body = Value::Object(body);
    crate::apply_openai_service_tier(&mut body, service_tier);
    body
}

/// Convert [`CreateMessageRequest::messages`] into the Responses
/// API `input[]` shape.
///
/// Each input item is one of four types:
///
/// * Simple user message: `{role: "user", content: "<text>"}` —
///   the "collapsed" form used when the user turn is a single
///   text block.
/// * Mixed user message: `{role: "user", content: [
///     {type: "input_text", text}, ... ]}` for multi-block user
///   turns.
/// * Assistant text: `{type: "message", role: "assistant",
///     content: [{type: "output_text", text}]}`. Assistant
///   messages are the only role with a `type` field; user
///   messages skip it.
/// * Assistant tool call: `{type: "function_call", call_id, name,
///     arguments: "<JSON-stringified>"}` — a **top-level** item
///   next to the assistant message, NOT nested inside it.
/// * Tool result: `{type: "function_call_output", call_id, output}`
///   — also top-level, emitted whenever the user turn contains
///   a `ToolResult` content block.
///
/// Text blocks are grouped with adjacent text blocks into a single
/// `message` item; a `ToolUse` content block ends the current
/// text run and starts its own `function_call` item. This
/// preserves the assistant's chronological interleaving of "say
/// something, then call a tool, then say something else" which
/// the engine's agentic loop relies on.
pub fn build_responses_input(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User => emit_user_message(msg, &mut out),
            Role::Assistant => emit_assistant_message(msg, &mut out),
            Role::System => {
                // System prompts are hoisted to top-level
                // `instructions` in build_responses_request_body —
                // skip here so we do not double-emit.
            }
        }
    }
    repair_orphaned_function_calls(&mut out);
    out
}

/// Ensure every `function_call` item in the input has a matching
/// `function_call_output`. The API rejects requests where a
/// `function_call` has no corresponding output — this can happen
/// when a tool call was interrupted (cancel/error during
/// execution) or the conversation history was truncated between
/// the call and its result.
///
/// Orphaned calls get a synthetic error output so the request
/// remains valid without discarding the rest of the history.
pub(super) fn repair_orphaned_function_calls(items: &mut Vec<Value>) {
    let mut call_ids = std::collections::HashSet::new();
    let mut output_ids = std::collections::HashSet::new();

    for item in items.iter() {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("function_call") => {
                if let Some(id) = item.get("call_id").and_then(|v| v.as_str()) {
                    call_ids.insert(id.to_string());
                }
            }
            Some("function_call_output") => {
                if let Some(id) = item.get("call_id").and_then(|v| v.as_str()) {
                    output_ids.insert(id.to_string());
                }
            }
            _ => {}
        }
    }

    // Forward: function_call without matching function_call_output —
    // inject a synthetic error output so the API accepts the request.
    let orphaned_calls: Vec<String> = call_ids.difference(&output_ids).cloned().collect();
    for call_id in &orphaned_calls {
        tracing::warn!(
            call_id = %call_id,
            "build_responses_input: injecting placeholder output for orphaned function_call"
        );
        items.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": "Error: tool execution was interrupted before producing output.",
        }));
    }

    // Reverse: function_call_output without matching function_call —
    // remove the orphaned output. This can happen when context pruning
    // (deduplication / truncation) drops a tool_use but the
    // corresponding tool_result survives in the protected suffix.
    let orphaned_outputs: std::collections::HashSet<String> =
        output_ids.difference(&call_ids).cloned().collect();
    if !orphaned_outputs.is_empty() {
        for id in &orphaned_outputs {
            tracing::warn!(
                call_id = %id,
                "build_responses_input: removing orphaned function_call_output (no matching function_call)"
            );
        }
        items.retain(|item| {
            if item.get("type").and_then(|v| v.as_str()) == Some("function_call_output") {
                if let Some(id) = item.get("call_id").and_then(|v| v.as_str()) {
                    return !orphaned_outputs.contains(id);
                }
            }
            true
        });
    }
}

fn image_block_to_responses_input_image(image: &ImageBlock) -> Value {
    let image_url = format!(
        "data:{};base64,{}",
        image.source.media_type, image.source.data
    );
    json!({
        "type": "input_image",
        "image_url": image_url,
    })
}

fn emit_tool_result_for_responses(tr: &crate::types::ToolResultBlock, out: &mut Vec<Value>) {
    match &tr.content {
        ToolResultContent::Text(text) => out.push(json!({
            "type": "function_call_output",
            "call_id": tr.tool_use_id,
            "output": text,
        })),
        ToolResultContent::Blocks(blocks) => {
            let mut output = Vec::new();
            let mut images = Vec::new();
            let mut documents = Vec::new();
            for block in blocks {
                match block {
                    ToolResultContentBlock::Text(text) => output.push(text.text.clone()),
                    ToolResultContentBlock::Image(image) => images.push(image),
                    ToolResultContentBlock::Document(document) => documents.push(document),
                }
            }

            out.push(json!({
                "type": "function_call_output",
                "call_id": tr.tool_use_id,
                "output": output.join("\n"),
            }));

            if !images.is_empty() || !documents.is_empty() {
                let mut content_parts = Vec::new();
                for line in output.iter().filter(|line| !line.is_empty()) {
                    content_parts.push(json!({
                        "type": "input_text",
                        "text": line,
                    }));
                }
                for image in images {
                    content_parts.push(image_block_to_responses_input_image(image));
                }
                for document in documents {
                    content_parts.push(json!({
                        "type": "input_file",
                        "filename": "document.pdf",
                        "file_data": format!(
                            "data:{};base64,{}",
                            document.source.media_type,
                            document.source.data
                        ),
                    }));
                }
                out.push(json!({
                    "role": "user",
                    "content": content_parts,
                }));
            }
        }
    }
}

pub(super) fn emit_user_message(msg: &Message, out: &mut Vec<Value>) {
    // Collect tool_result blocks separately; the remaining user
    // content stays in-order inside the message's content array.
    let mut content_parts: Vec<Value> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) => content_parts.push(json!({
                "type": "input_text",
                "text": t.text,
            })),
            ContentBlock::Image(img) => {
                content_parts.push(image_block_to_responses_input_image(img));
            }
            ContentBlock::ToolResult(tr) => emit_tool_result_for_responses(tr, out),
            // User messages cannot legally contain ToolUse or
            // Thinking blocks — silently skip so malformed
            // histories do not blow up the provider.
            _ => {}
        }
    }

    if content_parts.len() == 1 && content_parts[0]["type"] == "input_text" {
        out.push(json!({
            "role": "user",
            "content": content_parts[0]["text"],
        }));
    } else if !content_parts.is_empty() {
        out.push(json!({
            "role": "user",
            "content": content_parts,
        }));
    }
}

pub(super) fn emit_assistant_message(msg: &Message, out: &mut Vec<Value>) {
    // Walk the assistant's content blocks in order, flushing a
    // message item whenever we hit a tool_use so the text / tool
    // interleaving is preserved. This is load-bearing for the
    // agentic loop: "I'll read the file" (text) → Read(...)
    // (tool_call) → "and now I'll edit it" (text) → Edit(...)
    // (tool_call) must come back as four distinct input items.
    let mut pending_text: Vec<Value> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) => {
                pending_text.push(json!({
                    "type": "output_text",
                    "text": t.text,
                }));
            }
            ContentBlock::ToolUse(tu) => {
                flush_pending_text(&mut pending_text, out);
                // The Responses API wants `arguments` as a
                // JSON-encoded STRING, not the parsed object.
                let args_json =
                    serde_json::to_string(&tu.input).unwrap_or_else(|_| "{}".to_string());
                out.push(json!({
                    "type": "function_call",
                    "call_id": tu.id,
                    "name": tu.name,
                    "arguments": args_json,
                }));
            }
            ContentBlock::Thinking(tb) => {
                flush_pending_text(&mut pending_text, out);
                if let Some(encrypted_content) = tb.data.as_ref().filter(|data| !data.is_empty()) {
                    // The Responses API requires `summary` on every
                    // reasoning item — an array, empty when the model
                    // returned no summary text (common with
                    // `summary: auto`). Omitting it 400s with
                    // `missing_required_parameter: input[N].summary`.
                    let summary = if tb.thinking.is_empty() {
                        json!([])
                    } else {
                        json!([{
                            "type": "summary_text",
                            "text": tb.thinking,
                        }])
                    };
                    out.push(json!({
                        "type": "reasoning",
                        "encrypted_content": encrypted_content,
                        "summary": summary,
                    }));
                }
            }
            // Tool results in assistant messages are ill-formed.
            ContentBlock::ToolResult(_) => {}
            // A `compaction` item stands in for everything the server
            // summarised away, so it MUST be replayed — the history it
            // replaced is gone from our side. Only the encrypted
            // variant is a wire item; Anthropic's plaintext summary
            // belongs to a different provider's format.
            ContentBlock::Compaction(block) => {
                if let Some(encrypted_content) = block
                    .encrypted_content
                    .as_ref()
                    .filter(|content| !content.is_empty())
                {
                    flush_pending_text(&mut pending_text, out);
                    out.push(json!({
                        "type": "compaction",
                        "encrypted_content": encrypted_content,
                    }));
                }
            }
            // Server-side tool uses and their results are handled
            // inline by the provider — not replayed on subsequent turns.
            ContentBlock::ServerToolUse(_) | ContentBlock::WebSearchResult(_) => {}
            // Assistants don't generate images; drop if one appears.
            ContentBlock::Image(_) => {}
            // Server-side image_generation_call output is not replayed
            // on subsequent turns — the image bytes live on disk (or in
            // the transcript) and the model sees only the resulting
            // preview in the UI.
            ContentBlock::GeneratedImage(_) => {}
        }
    }
    flush_pending_text(&mut pending_text, out);
}

fn flush_pending_text(pending: &mut Vec<Value>, out: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending);
    out.push(json!({
        "type": "message",
        "role": "assistant",
        "content": content,
    }));
}

/// Translate Anthropic-shaped [`Tool`] declarations into the
/// Responses API tools[] shape.
///
/// Note the **flattened** layout: `{type: "function", name,
/// description, parameters}`. This is different from the
/// chat/completions format which nests everything under
/// `{type: "function", function: {name, description, parameters}}`.
pub fn build_responses_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })
        })
        .collect()
}
