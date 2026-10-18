//! DeepSeek's dialect of the Responses API.
//!
//! DeepSeek serves `POST {base}/responses` in the OpenAI Responses shape, but
//! the details that matter differ enough from OpenAI and Codex that the body
//! is built here rather than by flags threaded through the OpenAI builder:
//!
//! - The API is stateless: `store: false`, no `previous_response_id`, no
//!   `include`, no encrypted reasoning.
//! - Thinking mode rejects a tool-call turn whose reasoning is not replayed.
//!   The reasoning goes back as plaintext `reasoning_text` on exactly the turns
//!   that carry a tool call. The error that surfaces when it is missing is the
//!   misleading "No tool output found for tool call <id>".
//! - A user item that lands while a call is still unanswered ends the tool
//!   round early, with the same misleading error. So tool outputs go before a
//!   message's own text, and tool-result images wait until every output is in.
//! - Effort reads `none` / `low` / `high` / `max`; `xhigh` would run as `high`,
//!   so it is sent as `max`, and disabled thinking is `effort: "none"`.
//!
//! This was the `deepseek-responses` plugin until it moved in-tree; the
//! behaviour, and the reasons for it, are the plugin's.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::openai::OpenAiRequestOptions;
use crate::request::{CreateMessageRequest, ReasoningEffort, ThinkingConfig};
use crate::types::{
    ContentBlock, ImageBlock, Message, Role, ToolChoice, ToolResultContent, ToolResultContentBlock,
};

/// Default base URL when the entry leaves it empty and `DEEPSEEK_BASE_URL` is
/// unset.
pub const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

/// Option keys that configure reasoning rather than ride the body as-is.
/// Chat Completions-style `reasoning_effort` and `thinking: {type}` are what
/// rebon's `thinkingEnabled` / `thinkingEffort` settings write; they are
/// translated into `reasoning` and never sent raw. `rebonAnchoredPreset`
/// belonged to the plugin's experimental seeded preset, which did not move
/// in-tree; it is dropped so a migrated entry does not send it upstream.
const REASONING_OPTION_KEYS: &[&str] = &["reasoning", "reasoning_effort", "thinking"];
const DROPPED_OPTION_KEYS: &[&str] = &["rebonAnchoredPreset"];
/// Fields an option may not overwrite: they are the request itself.
const PROTECTED_FIELDS: &[&str] = &["model", "input", "stream", "tools"];

const IMAGE_OUTSIDE_USER_TEXT: &str = "[image omitted: images are only accepted in user messages]";
const DOCUMENT_TEXT: &str = "[document omitted: not supported by DeepSeek]";

/// The `/responses` URL for a DeepSeek base.
///
/// Unlike the OpenAI surface, a bare origin gets no `/v1`: DeepSeek serves
/// `https://api.deepseek.com/responses`.
pub fn deepseek_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{base}/responses")
    }
}

/// The DeepSeek request body for one turn.
///
/// `provider_options` and `model_options` are the entry's `options` and its
/// per-model options, applied in that order: each layer removes its
/// `omitBodyFields`, then merges `body`, then `extraBody`. Keys are DeepSeek
/// wire names, which gives users an escape hatch for dialect drift.
pub fn build_deepseek_request_body(
    request: &CreateMessageRequest,
    prompt_cache_key: Option<&str>,
    provider_options: &OpenAiRequestOptions,
    model_options: Option<&OpenAiRequestOptions>,
) -> Value {
    let input = build_deepseek_input(&request.messages_with_transient_context());

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    if let Some(system) = request.system.as_deref().filter(|s| !s.is_empty()) {
        body.insert("instructions".into(), json!(system));
    }
    if request.max_tokens > 0 {
        body.insert("max_output_tokens".into(), json!(request.max_tokens));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }

    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect();
    if request.web_search.is_some() {
        tools.push(json!({ "type": "web_search" }));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = request.tool_choice.as_ref().and_then(tool_choice) {
        body.insert("tool_choice".into(), choice);
    }

    let layers: Vec<&OpenAiRequestOptions> = std::iter::once(provider_options)
        .chain(model_options)
        .collect();
    let reasoning = resolve_reasoning(request, &layers);
    if let Some(reasoning) = &reasoning {
        body.insert("reasoning".into(), reasoning.value.clone());
    }

    // The official endpoint ignores `prompt_cache_key`; gateways with keyed
    // prefix caches route and retain by it. `omitBodyFields` opts out.
    if let Some(key) = prompt_cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }

    for layer in &layers {
        apply_option_layer(&mut body, layer);
    }
    // An explicit `reasoning` object is the user's word, so it survives that
    // layer's own `omitBodyFields`.
    if let Some(reasoning) = reasoning.filter(|r| r.explicit) {
        body.insert("reasoning".into(), reasoning.value);
    }

    Value::Object(body)
}

fn apply_option_layer(body: &mut Map<String, Value>, options: &OpenAiRequestOptions) {
    for field in &options.omit_body_fields {
        if !PROTECTED_FIELDS.contains(&field.as_str()) {
            body.remove(field);
        }
    }
    for source in [&options.body, &options.extra_body] {
        for (key, value) in source {
            let key = key.as_str();
            if REASONING_OPTION_KEYS.contains(&key)
                || DROPPED_OPTION_KEYS.contains(&key)
                || PROTECTED_FIELDS.contains(&key)
            {
                continue;
            }
            body.insert(key.to_string(), value.clone());
        }
    }
}

fn tool_choice(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Auto => Some(json!("auto")),
        ToolChoice::Any => Some(json!("required")),
        ToolChoice::None => Some(json!("none")),
        ToolChoice::Tool { name } => Some(json!({ "type": "function", "name": name })),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// DeepSeek's effort for a rebon effort. `xhigh` becomes `max`, because left
/// to the server it would run as `high`; `medium` is forwarded for the
/// endpoint's own fallback.
pub fn deepseek_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh | ReasoningEffort::Max => "max",
    }
}

/// An effort written in an option, folded the same way. Unknown words are
/// sent as written: the user typed them for this endpoint.
fn option_effort(word: &str) -> String {
    match word {
        "xhigh" => "max".to_string(),
        other => other.to_string(),
    }
}

struct ResolvedReasoning {
    value: Value,
    explicit: bool,
}

/// What one option layer says about reasoning.
#[derive(Default)]
struct LayerReasoning {
    explicit: Option<Value>,
    effort: Option<String>,
}

fn layer_reasoning(options: &OpenAiRequestOptions) -> LayerReasoning {
    let mut out = LayerReasoning::default();
    for source in [&options.body, &options.extra_body] {
        if let Some(explicit) = source.get("reasoning") {
            out.explicit = Some(explicit.clone());
        }
        if let Some(effort) = source.get("reasoning_effort").and_then(Value::as_str) {
            out.effort = Some(option_effort(effort));
        }
        let thinking_type = source
            .get("thinking")
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str);
        if thinking_type == Some("disabled") {
            out.effort = Some("none".to_string());
        }
    }
    out
}

/// The `reasoning` field, highest precedence first: an explicit `reasoning`
/// object in the options (per-model beats provider), the turn's own thinking
/// switch, the turn's effort, a per-model option effort, a provider option
/// effort. Nothing set leaves the field off and the server default applies.
fn resolve_reasoning(
    request: &CreateMessageRequest,
    layers: &[&OpenAiRequestOptions],
) -> Option<ResolvedReasoning> {
    let per_layer: Vec<LayerReasoning> =
        layers.iter().map(|layer| layer_reasoning(layer)).collect();

    if let Some(explicit) = per_layer
        .iter()
        .rev()
        .find_map(|layer| layer.explicit.clone())
    {
        let value = match explicit {
            Value::Object(mut map) => {
                if let Some(effort) = map.get("effort").and_then(Value::as_str) {
                    let folded = option_effort(effort);
                    map.insert("effort".into(), json!(folded));
                }
                Value::Object(map)
            }
            other => other,
        };
        return Some(ResolvedReasoning {
            value,
            explicit: true,
        });
    }

    let effort = if matches!(request.thinking, Some(ThinkingConfig::Disabled)) {
        Some("none".to_string())
    } else if let Some(effort) = request.reasoning_effort {
        Some(deepseek_effort(effort).to_string())
    } else {
        per_layer
            .iter()
            .rev()
            .find_map(|layer| layer.effort.clone())
    };
    effort.map(|effort| ResolvedReasoning {
        value: json!({ "effort": effort }),
        explicit: false,
    })
}

/// Translate rebon history into DeepSeek `input` items.
pub fn build_deepseek_input(messages: &[Message]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            // Hoisted into `instructions`.
            Role::System => continue,
        };
        if let Some(reasoning) = replayed_reasoning(message) {
            input.push(reasoning);
        }
        emit_message(role, message, &mut input);
    }
    input
}

/// The reasoning item replayed ahead of an assistant turn, when it must be.
///
/// Only turns that carry a tool call: those are the ones DeepSeek rejects
/// without it, and on plain turns the server ignores it. The id comes from
/// the turn's first call so a re-sent history keeps a byte-identical prefix.
fn replayed_reasoning(message: &Message) -> Option<Value> {
    if message.role != Role::Assistant {
        return None;
    }
    let first_call = message.content.iter().find_map(|block| match block {
        ContentBlock::ToolUse(call) => Some(call.id.as_str()),
        _ => None,
    })?;
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking(thinking) if !thinking.thinking.is_empty() => {
                Some(thinking.thinking.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    Some(json!({
        "type": "reasoning",
        "id": format!("rs_{first_call}"),
        "status": "completed",
        "summary": [],
        "content": [{ "type": "reasoning_text", "text": text }],
    }))
}

/// One `{role, content}` item per contiguous run of content, so a turn that
/// interleaves text and tool calls keeps its text where it was.
struct Run<'a> {
    role: &'a str,
    text: Vec<String>,
    parts: Vec<Value>,
}

impl Run<'_> {
    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let kind = if self.role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        self.parts
            .push(json!({ "type": kind, "text": self.text.join("\n") }));
        self.text.clear();
    }

    fn flush(&mut self, input: &mut Vec<Value>) {
        self.flush_text();
        if self.parts.is_empty() {
            return;
        }
        input.push(json!({ "role": self.role, "content": std::mem::take(&mut self.parts) }));
    }
}

fn emit_message(role: &str, message: &Message, input: &mut Vec<Value>) {
    // User text opens a new round, so the previous round's tool results go
    // first; DeepSeek otherwise decides they are missing.
    let blocks: Vec<&ContentBlock> = if role == "user" {
        let (results, rest): (Vec<&ContentBlock>, Vec<&ContentBlock>) = message
            .content
            .iter()
            .partition(|block| matches!(block, ContentBlock::ToolResult(_)));
        results.into_iter().chain(rest).collect()
    } else {
        message.content.iter().collect()
    };

    let mut run = Run {
        role,
        text: Vec::new(),
        parts: Vec::new(),
    };
    // Images from tool results wait until every output in this message is
    // in: a user item between two outputs ends the tool round early.
    let mut held_images: Vec<Value> = Vec::new();
    let flush_held = |held: &mut Vec<Value>, input: &mut Vec<Value>| {
        if !held.is_empty() {
            input.push(json!({ "role": "user", "content": std::mem::take(held) }));
        }
    };

    for block in blocks {
        if !matches!(block, ContentBlock::ToolResult(_)) {
            flush_held(&mut held_images, input);
        }
        match block {
            ContentBlock::Text(text) => run.text.push(text.text.clone()),
            ContentBlock::Image(image) => {
                if role == "user" {
                    run.flush_text();
                    run.parts.push(input_image(image));
                } else {
                    // Only user messages may carry `input_image`; the
                    // endpoint 400s on one anywhere else.
                    run.text.push(IMAGE_OUTSIDE_USER_TEXT.to_string());
                }
            }
            ContentBlock::Thinking(_) => {
                // Replayed ahead of the turn when it calls a tool, otherwise
                // deliberately dropped.
            }
            ContentBlock::ToolUse(call) => {
                run.flush(input);
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".into()),
                }));
            }
            ContentBlock::ToolResult(result) => {
                run.flush(input);
                let (lines, images) = split_tool_result(&result.content);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": result.tool_use_id,
                    "output": lines.join("\n"),
                }));
                if !images.is_empty() {
                    held_images.extend(
                        lines
                            .iter()
                            .filter(|line| !line.is_empty())
                            .map(|line| json!({ "type": "input_text", "text": line })),
                    );
                    held_images.extend(images.into_iter().map(input_image));
                }
            }
            ContentBlock::ServerToolUse(call) => {
                // The server restores the search results from the call id.
                run.flush(input);
                let action = if call.input.is_null() {
                    json!({})
                } else {
                    call.input.clone()
                };
                input.push(json!({
                    "type": "web_search_call",
                    "id": call.id,
                    "status": "completed",
                    "action": action,
                }));
            }
            // Restored server-side from the replayed `web_search_call`.
            ContentBlock::WebSearchResult(_) => {}
            ContentBlock::Compaction(compaction) => {
                if let Some(content) = compaction.content.as_deref().filter(|c| !c.is_empty()) {
                    run.text.push(content.to_string());
                }
            }
            ContentBlock::GeneratedImage(_) => {}
        }
    }
    flush_held(&mut held_images, input);
    run.flush(input);
}

fn split_tool_result(content: &ToolResultContent) -> (Vec<String>, Vec<&ImageBlock>) {
    match content {
        ToolResultContent::Text(text) => (vec![text.clone()], Vec::new()),
        ToolResultContent::Blocks(blocks) => {
            let mut lines = Vec::new();
            let mut images = Vec::new();
            for block in blocks {
                match block {
                    ToolResultContentBlock::Text(text) => lines.push(text.text.clone()),
                    ToolResultContentBlock::Document(_) => lines.push(DOCUMENT_TEXT.to_string()),
                    ToolResultContentBlock::Image(image) => images.push(image),
                }
            }
            (lines, images)
        }
    }
}

/// The endpoint reads the format from the bytes; `detail` is left off because
/// its default already means "keep the original".
fn input_image(image: &ImageBlock) -> Value {
    json!({
        "type": "input_image",
        "image_url": format!("data:{};base64,{}", image.source.media_type, image.source.data),
    })
}

/// Append one outgoing body to the file named by `REBON_DEEPSEEK_DUMP`.
///
/// Off unless the variable is set. Wire bugs in this dialect surface as
/// upstream 400s whose text names the wrong cause, and the body actually sent
/// is the only thing that settles them. Never fails the turn.
pub fn dump_request_body(body: &Value) {
    let Some(path) = std::env::var_os("REBON_DEEPSEEK_DUMP").filter(|p| !p.is_empty()) else {
        return;
    };
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = json!({ "at_ms": at_ms, "body": body });
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| {
            use std::io::Write;
            writeln!(file, "{line}")
        });
    if let Err(error) = result {
        tracing::warn!(%error, "deepseek: request dump failed");
    }
}

/// Per-model options for `model`, if any.
pub fn model_options<'a>(
    options: &'a BTreeMap<String, OpenAiRequestOptions>,
    model: &str,
) -> Option<&'a OpenAiRequestOptions> {
    options.get(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(value: Value) -> Vec<Message> {
        serde_json::from_value(value).expect("fixture messages")
    }

    fn image(data: &str) -> Value {
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}})
    }

    fn options(value: Value) -> OpenAiRequestOptions {
        let object = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
        };
        OpenAiRequestOptions {
            body: object("body"),
            extra_body: object("extraBody"),
            omit_body_fields: value
                .get("omitBodyFields")
                .and_then(Value::as_array)
                .map(|fields| {
                    fields
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn body(request: &CreateMessageRequest) -> Value {
        build_deepseek_request_body(request, None, &OpenAiRequestOptions::default(), None)
    }

    fn simple() -> CreateMessageRequest {
        CreateMessageRequest::simple("deepseek-v4-pro", "hi")
    }

    #[test]
    fn endpoint_has_no_v1_and_keeps_an_explicit_suffix() {
        assert_eq!(
            deepseek_endpoint("https://api.deepseek.com"),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            deepseek_endpoint("https://api.deepseek.com/"),
            "https://api.deepseek.com/responses"
        );
        assert_eq!(
            deepseek_endpoint("https://relay.example/v1/responses"),
            "https://relay.example/v1/responses"
        );
    }

    #[test]
    fn body_is_stateless_and_carries_no_openai_only_fields() {
        let mut request = simple();
        request.system = Some("be brief".into());
        request.max_tokens = 2048;
        let body = body(&request);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["max_output_tokens"], 2048);
        for absent in [
            "include",
            "previous_response_id",
            "reasoning",
            "prompt_cache_key",
            "service_tier",
        ] {
            assert!(body.get(absent).is_none(), "{absent} must not be sent");
        }
    }

    #[test]
    fn tools_tool_choice_and_web_search() {
        let mut request = simple();
        request.tools = vec![serde_json::from_value(json!({
            "name": "Bash", "description": "run", "input_schema": {"type": "object"},
        }))
        .unwrap()];
        request.tool_choice = Some(ToolChoice::Any);
        request.web_search = Some(crate::request::WebSearchToolConfig {
            allowed_domains: None,
            blocked_domains: None,
            max_uses: None,
            search_context_size: None,
            user_location: None,
        });
        let sent = body(&request);
        assert_eq!(
            sent["tools"][0],
            json!({"type": "function", "name": "Bash", "description": "run", "parameters": {"type": "object"}})
        );
        assert_eq!(sent["tools"][1], json!({"type": "web_search"}));
        assert_eq!(sent["tool_choice"], "required");

        request.tool_choice = Some(ToolChoice::Tool {
            name: "Bash".into(),
        });
        assert_eq!(
            body(&request)["tool_choice"],
            json!({"type": "function", "name": "Bash"})
        );
    }

    #[test]
    fn effort_folds_xhigh_to_max_and_disabled_thinking_to_none() {
        let mut request = simple();
        request.reasoning_effort = Some(ReasoningEffort::XHigh);
        assert_eq!(body(&request)["reasoning"], json!({"effort": "max"}));
        request.reasoning_effort = Some(ReasoningEffort::High);
        assert_eq!(body(&request)["reasoning"], json!({"effort": "high"}));
        request.thinking = Some(ThinkingConfig::Disabled);
        assert_eq!(
            body(&request)["reasoning"],
            json!({"effort": "none"}),
            "the off switch wins over the turn effort"
        );
    }

    #[test]
    fn reasoning_option_precedence() {
        let request = simple();
        let provider = options(json!({"body": {"reasoning_effort": "low"}}));
        let model = options(json!({"extraBody": {"reasoning_effort": "xhigh"}}));
        let sent = build_deepseek_request_body(&request, None, &provider, Some(&model));
        assert_eq!(
            sent["reasoning"],
            json!({"effort": "max"}),
            "per-model option beats provider option"
        );
        assert!(
            sent.get("reasoning_effort").is_none() && sent.get("thinking").is_none(),
            "chat-style keys are never sent raw"
        );

        let disabled = options(
            json!({"body": {"thinking": {"type": "disabled"}, "reasoning_effort": "high"}}),
        );
        let sent = build_deepseek_request_body(&request, None, &disabled, None);
        assert_eq!(sent["reasoning"], json!({"effort": "none"}));

        let mut turn = request.clone();
        turn.reasoning_effort = Some(ReasoningEffort::Low);
        let sent = build_deepseek_request_body(&turn, None, &provider, Some(&model));
        assert_eq!(
            sent["reasoning"],
            json!({"effort": "low"}),
            "the turn effort beats option efforts"
        );

        let explicit = options(json!({
            "body": {"reasoning": {"effort": "xhigh", "summary": "auto"}},
            "omitBodyFields": ["reasoning"],
        }));
        let sent = build_deepseek_request_body(&turn, None, &explicit, None);
        assert_eq!(
            sent["reasoning"],
            json!({"effort": "max", "summary": "auto"}),
            "explicit reasoning wins and survives omitBodyFields"
        );

        let omit = options(json!({"omitBodyFields": ["reasoning"]}));
        assert!(build_deepseek_request_body(&turn, None, &omit, None)
            .get("reasoning")
            .is_none());
    }

    #[test]
    fn option_layers_merge_in_order_and_skip_reserved_keys() {
        let request = simple();
        let provider = options(json!({
            "extraBody": {"top_p": 0.1, "prompt_cache_retention": "24h", "rebonAnchoredPreset": "seeded"},
            "omitBodyFields": ["temperature"],
        }));
        let model = options(json!({"body": {"top_p": 0.5, "model": "evil"}}));
        let sent = build_deepseek_request_body(&request, Some("key-1"), &provider, Some(&model));
        assert_eq!(sent["top_p"], 0.5, "the per-model layer applies last");
        assert_eq!(sent["prompt_cache_retention"], "24h");
        assert_eq!(sent["prompt_cache_key"], "key-1");
        assert_eq!(sent["model"], "deepseek-v4-pro");
        assert!(sent.get("rebonAnchoredPreset").is_none());

        let no_key = options(json!({"omitBodyFields": ["prompt_cache_key"]}));
        assert!(
            build_deepseek_request_body(&request, Some("key-1"), &no_key, None)
                .get("prompt_cache_key")
                .is_none()
        );
    }

    #[test]
    fn reasoning_is_replayed_only_on_tool_call_turns_with_a_stable_id() {
        let input = build_deepseek_input(&messages(json!([
            {"role": "user", "content": [{"type": "text", "text": "q"}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan"},
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"command": "ls"}},
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "file.txt"}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "done"},
                {"type": "text", "text": "bye"},
            ]},
        ])));
        assert_eq!(
            input,
            vec![
                json!({"role": "user", "content": [{"type": "input_text", "text": "q"}]}),
                json!({"type": "reasoning", "id": "rs_call_1", "status": "completed", "summary": [],
                       "content": [{"type": "reasoning_text", "text": "plan"}]}),
                json!({"role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}),
                json!({"type": "function_call", "call_id": "call_1", "name": "Bash",
                       "arguments": "{\"command\":\"ls\"}"}),
                json!({"type": "function_call_output", "call_id": "call_1", "output": "file.txt"}),
                json!({"role": "assistant", "content": [{"type": "output_text", "text": "bye"}]}),
            ]
        );
    }

    #[test]
    fn tool_results_go_before_user_text() {
        let input = build_deepseek_input(&messages(json!([
            {"role": "assistant", "content": [{"type": "tool_use", "id": "c1", "name": "Read", "input": {}}]},
            {"role": "user", "content": [
                {"type": "text", "text": "[Request interrupted by user]"},
                {"type": "tool_result", "tool_use_id": "c1", "content": "x"},
                {"type": "text", "text": "go on"},
            ]},
        ])));
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(
            input[2],
            json!({"role": "user", "content": [
                {"type": "input_text", "text": "[Request interrupted by user]\ngo on"},
            ]})
        );
    }

    #[test]
    fn parallel_image_results_send_every_output_before_any_image() {
        let input = build_deepseek_input(&messages(json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_01_b", "content": [image("BBBB")]},
                {"type": "tool_result", "tool_use_id": "call_00_a",
                 "content": [{"type": "text", "text": "frame a"}, image("AAAA")]},
                {"type": "text", "text": "go on"},
            ]},
        ])));
        assert_eq!(
            input,
            vec![
                json!({"type": "function_call_output", "call_id": "call_01_b", "output": ""}),
                json!({"type": "function_call_output", "call_id": "call_00_a", "output": "frame a"}),
                json!({"role": "user", "content": [
                    {"type": "input_image", "image_url": "data:image/png;base64,BBBB"},
                    {"type": "input_text", "text": "frame a"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                ]}),
                json!({"role": "user", "content": [{"type": "input_text", "text": "go on"}]}),
            ]
        );
    }

    #[test]
    fn images_outside_user_messages_and_documents_become_text() {
        let input = build_deepseek_input(&messages(json!([
            {"role": "assistant", "content": [image("EEEE"), {"type": "text", "text": "see above"}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "d", "content": [
                {"type": "text", "text": "report"},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "ZZ"}},
            ]}]},
        ])));
        assert_eq!(
            input[0],
            json!({"role": "assistant", "content": [
                {"type": "output_text", "text": format!("{IMAGE_OUTSIDE_USER_TEXT}\nsee above")},
            ]})
        );
        assert_eq!(
            input[1],
            json!({"type": "function_call_output", "call_id": "d",
                   "output": format!("report\n{DOCUMENT_TEXT}")})
        );
        assert_eq!(input.len(), 2, "a document adds no image message");
    }

    #[test]
    fn web_search_replays_as_its_call_and_drops_the_results() {
        let input = build_deepseek_input(&messages(json!([
            {"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "ws_1", "name": "web_search", "input": {"query": "rust"}},
                {"type": "web_search_result", "tool_use_id": "ws_1", "content": []},
                {"type": "text", "text": "answer"},
            ]},
        ])));
        assert_eq!(
            input,
            vec![
                json!({"type": "web_search_call", "id": "ws_1", "status": "completed",
                       "action": {"query": "rust"}}),
                json!({"role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}),
            ]
        );
    }
}
