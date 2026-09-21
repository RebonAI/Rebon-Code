//! Stream translation for the plugin request-routing path: the layer
//! that lets a plugin llm adapter (dsh `StreamChunk` protocol) serve
//! rebon model requests.
//!
//! [`DshLlmClient`] implements [`ModelClient`] over a
//! whichever runtime holds the adapter; the chunk↔event mapping follows
//! the table in the design doc, including the two extra dsh contracts:
//! an empty completion is a retryable error (EMPTY_RESPONSE), and failures
//! keep their channel (adapter throw vs. in-band `finish {reason:'error'}`)
//! when folded into [`ModelError`].
//!
//! Request-direction fidelity (calibrated against the real
//! llm-deepseek adapter): the rebon [`CreateMessageRequest`] is translated
//! into the dsh `GenerateOptions` vocabulary — messages become dsh content
//! blocks (`text`/`reasoning`/`tool-call`/`tool-result`), tools become
//! `{name, description, parameters}` schemas, and the thinking/effort
//! configuration folds into dsh's `off | high | max` reasoning effort.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Poll};

use async_trait::async_trait;
use futures_util::Stream;
use rebon_api::{
    ContentBlock, ContentBlockDelta, ContentBlockStart, CreateMessageRequest, Message,
    MessageDeltaFields, ModelClient, ModelError, ModelResult, Role, StopReason, StreamEvent,
    StreamEventStream, Usage,
};
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::{
    PluginHostSupervisor, ServiceStream, StreamEvent as SupervisorStreamEvent,
};
use tokio::sync::mpsc;

/// Where one dsh adapter lives on the plugin plane.
///
/// This used to be a trait with a stream of JSON envelopes behind it —
/// `{chunk}` per piece, then `{done}` or `{err, code?}` — because the adapter
/// could live in a second runtime or on the plugin plane, and the router
/// was not supposed to know which. The second runtime is gone; the plane is
/// the only place an adapter has run for some time, and the envelope was a
/// second stream shape wrapping the one the supervisor already hands out.
///
/// So the seam is a plain address now. Both plugin model paths make the same
/// `llm/stream` call: what still differs between them is the vocabulary in
/// the payload — dsh `StreamChunk`s here, `StreamEventV1` in
/// [`crate::plane_model_provider`] — and that is a fact about the adapters,
/// not about the transport.
#[derive(Clone, Debug)]
pub struct LlmRoute {
    pub supervisor: Arc<PluginHostSupervisor>,
    pub plugin_id: String,
    pub provider: String,
    pub scope: String,
}

/// Process-wide adapter route registry: provider route name → the plugin
/// serving it. The plane registers one for every `llmProviders` entry a
/// plugin declares.
fn registry() -> &'static Mutex<HashMap<String, Arc<LlmRoute>>> {
    static ROUTES: OnceLock<Mutex<HashMap<String, Arc<LlmRoute>>>> = OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_llm_host(provider: impl Into<String>, route: Arc<LlmRoute>) {
    registry()
        .lock()
        .expect("llm route registry poisoned")
        .insert(provider.into(), route);
}

pub fn unregister_llm_host(provider: &str) {
    registry()
        .lock()
        .expect("llm route registry poisoned")
        .remove(provider);
}

pub fn llm_host_for(provider: &str) -> Option<Arc<LlmRoute>> {
    registry()
        .lock()
        .expect("llm route registry poisoned")
        .get(provider)
        .cloned()
}

/// [`ModelClient`] served by a dsh adapter on the plugin plane.
pub struct DshLlmClient {
    provider: String,
    route: Arc<LlmRoute>,
}

impl DshLlmClient {
    pub fn new(provider: impl Into<String>, route: Arc<LlmRoute>) -> Self {
        Self {
            provider: provider.into(),
            route,
        }
    }
}

#[async_trait]
impl ModelClient for DshLlmClient {
    fn provider_name(&self) -> &'static str {
        "plugin-llm"
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        let model = request.model.clone();
        let payload = dsh_generate_payload(&self.provider, &request);
        let stream = self
            .route
            .supervisor
            .stream_llm(
                &self.route.plugin_id,
                &self.route.scope,
                &self.route.provider,
                Payload::from(payload),
            )
            .await
            .map_err(|error| ModelError::other(format!("plugin llm host: {error}")))?;
        let call_id = stream.call_id().to_owned();
        let (tx, rx) = mpsc::unbounded_channel();
        // Translated in the pump rather than in `poll_next`: a `Stream` that
        // builds its receive future inside a poll drops the waker the last one
        // registered, and the turn then stops after its first `Pending`.
        tokio::spawn(pump(
            TranslateState::new(format!("plugin-{}-{call_id}", self.provider), model),
            stream,
            tx,
        ));
        Ok(Box::pin(TranslatedStream {
            rx,
            supervisor: Arc::clone(&self.route.supervisor),
            call_id,
            finished: false,
        }))
    }
}

/// Turns one plugin call's dsh chunks into rebon events until it ends.
///
/// The three ways a turn can end, each preserved from the envelope this
/// replaced: a terminal with a dsh `finish` already seen simply stops; one
/// without is a protocol error, because a dsh adapter that completes without
/// finishing has told the model layer nothing about why; and a failed
/// terminal keeps its bracketed code, which is what the model layer routes
/// retries on.
async fn pump(
    mut state: TranslateState,
    mut stream: ServiceStream,
    tx: mpsc::UnboundedSender<ModelResult<StreamEvent>>,
) {
    let mut queue: VecDeque<ModelResult<StreamEvent>> = VecDeque::new();
    while let Some(event) = stream.recv().await {
        match event {
            SupervisorStreamEvent::Chunk(payload) => {
                let chunk = payload.to_value().unwrap_or(serde_json::Value::Null);
                translate_chunk(&mut state, &chunk, &mut queue);
            }
            SupervisorStreamEvent::End(Ok(_)) => {
                if !state.finished {
                    queue.push_back(Err(ModelError::Protocol(
                        "plugin adapter completed without a finish chunk".into(),
                    )));
                }
                drain(&mut queue, &tx);
                return;
            }
            SupervisorStreamEvent::End(Err(error)) => {
                queue.push_back(Err(ModelError::Permanent(format!(
                    "plugin adapter stream failed: {error}"
                ))));
                drain(&mut queue, &tx);
                return;
            }
        }
        if !drain(&mut queue, &tx) {
            return;
        }
    }
    // The call's own stream ended without a terminal — the host went away
    // mid-turn. The third of the three endings, and the one the reader used
    // to detect for itself before the pump was the only thing that could see
    // it.
    queue.push_back(Err(ModelError::Protocol(
        "plugin adapter closed the stream without finishing".into(),
    )));
    drain(&mut queue, &tx);
}

/// Hands the queue to the reader. `false` once nobody is reading.
fn drain(
    queue: &mut VecDeque<ModelResult<StreamEvent>>,
    tx: &mpsc::UnboundedSender<ModelResult<StreamEvent>>,
) -> bool {
    while let Some(item) = queue.pop_front() {
        if tx.send(item).is_err() {
            return false;
        }
    }
    true
}

/// rebon request → dsh `GenerateOptions`-shaped JSON (request direction of
/// the request-direction mapping table, calibrated against dsh's serialize.ts).
fn dsh_generate_payload(provider: &str, request: &CreateMessageRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request
        .messages_with_transient_context()
        .iter()
        .map(dsh_message)
        .collect();
    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect();

    let mut payload = serde_json::json!({
        "provider": provider,
        "model": request.model,
        "system": request.system,
        "messages": messages,
        "tools": tools,
        "maxTokens": request.max_tokens,
        "temperature": request.temperature,
    });
    let map = payload.as_object_mut().expect("payload is an object");
    if !request.stop_sequences.is_empty() {
        map.insert(
            "stop".into(),
            serde_json::json!(request.stop_sequences.clone()),
        );
    }
    if let Some(effort) = dsh_reasoning_effort(request) {
        map.insert("reasoningEffort".into(), serde_json::json!(effort));
    }
    payload
}

/// One rebon [`Message`] as a dsh message (content-block vocabulary).
fn dsh_message(message: &Message) -> serde_json::Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    };
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(text) => {
                content.push(serde_json::json!({ "type": "text", "text": text.text }));
            }
            ContentBlock::Thinking(thinking) => {
                // A redacted block carries no replayable text for a foreign
                // provider; only real thinking text crosses.
                if !thinking.thinking.is_empty() {
                    content.push(serde_json::json!({
                        "type": "reasoning",
                        "text": thinking.thinking,
                    }));
                }
            }
            ContentBlock::ToolUse(tool_use) => {
                content.push(serde_json::json!({
                    "type": "tool-call",
                    "id": tool_use.id,
                    "name": tool_use.name,
                    // dsh carries arguments as the model's raw JSON string.
                    "arguments": tool_use.input.to_string(),
                }));
            }
            ContentBlock::ToolResult(result) => {
                let mut entry = serde_json::json!({
                    "type": "tool-result",
                    "toolCallId": result.tool_use_id,
                    "content": [{ "type": "text", "text": result.content.to_plain_text() }],
                });
                if result.is_error {
                    entry["isError"] = serde_json::json!(true);
                }
                content.push(entry);
            }
            // Text-only wire route: the block reaches dsh as a bare image
            // marker so its serializer refuses with the explicit
            // UNSUPPORTED_CONTENT diagnosis instead of silently dropping it.
            ContentBlock::Image(_) => {
                content.push(serde_json::json!({ "type": "image" }));
            }
            ContentBlock::Compaction(compaction) => {
                if let Some(text) = compaction.content.as_deref().filter(|t| !t.is_empty()) {
                    content.push(serde_json::json!({ "type": "text", "text": text }));
                }
            }
            // Provider-executed artifacts of other backends; no dsh
            // projection exists and the model must not re-see them.
            ContentBlock::ServerToolUse(_)
            | ContentBlock::WebSearchResult(_)
            | ContentBlock::GeneratedImage(_) => {}
        }
    }
    serde_json::json!({ "role": role, "content": content })
}

/// Fold rebon thinking/effort configuration into dsh's `off | high | max`.
/// The vocabulary is declarative profile DATA now
/// ([`rebon_core::provider_profiles::DEEPSEEK_PROFILE`]); this wrapper
/// keeps the dispatch call sites (and their tests) unchanged.
fn dsh_reasoning_effort(request: &CreateMessageRequest) -> Option<&'static str> {
    rebon_core::provider_profiles::DEEPSEEK_PROFILE.reasoning_label(request)
}

/// What we know about a content block index mid-stream.
#[derive(Clone, Copy, PartialEq)]
enum BlockState {
    /// `block-start` for a tool call arrived, but the id/name live in the
    /// first `tool-call-delta`, so the rebon start event is still pending.
    PendingTool,
    /// A start event has been emitted for this index.
    Started,
}

struct TranslateState {
    message_id: String,
    model: String,
    started: bool,
    finished: bool,
    saw_content: bool,
    blocks: HashMap<usize, BlockState>,
    usage: Usage,
}

impl TranslateState {
    fn new(message_id: String, model: String) -> Self {
        Self {
            message_id,
            model,
            started: false,
            finished: false,
            saw_content: false,
            blocks: HashMap::new(),
            usage: Usage::default(),
        }
    }
}

struct TranslatedStream {
    rx: mpsc::UnboundedReceiver<ModelResult<StreamEvent>>,
    supervisor: Arc<PluginHostSupervisor>,
    call_id: String,
    finished: bool,
}

impl Drop for TranslatedStream {
    fn drop(&mut self) {
        // rebon cancellation = dropping the stream. Tell the adapter so it can
        // abort its request (dsh answers with `finish {reason:'aborted'}`,
        // which nobody is left to read). The supervisor's own `Drop` closes
        // the call's accounting but sends nothing, so the cancel is here.
        if self.finished {
            return;
        }
        let supervisor = Arc::clone(&self.supervisor);
        let call_id = std::mem::take(&mut self.call_id);
        tokio::spawn(async move {
            let _ = supervisor.cancel(&call_id).await;
        });
    }
}

impl Stream for TranslatedStream {
    type Item = ModelResult<StreamEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(item)) => {
                // An error terminates the stream contract, and the pump has
                // already stopped; do not also fire a cancel for a turn that
                // is over.
                if item.is_err() {
                    this.finished = true;
                }
                Poll::Ready(Some(item))
            }
            // The pump is the only thing that can tell a clean ending from a
            // torn one, and it has already said which this was; a closed
            // channel is simply the end.
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn ensure_started(state: &mut TranslateState, out: &mut VecDeque<ModelResult<StreamEvent>>) {
    if !state.started {
        state.started = true;
        out.push_back(Ok(StreamEvent::MessageStart {
            message_id: state.message_id.clone(),
            model: state.model.clone(),
            usage: Usage::default(),
        }));
    }
}

fn ensure_block(
    state: &mut TranslateState,
    out: &mut VecDeque<ModelResult<StreamEvent>>,
    index: usize,
    start: impl FnOnce() -> ContentBlockStart,
) {
    if state.blocks.get(&index) != Some(&BlockState::Started) {
        state.blocks.insert(index, BlockState::Started);
        out.push_back(Ok(StreamEvent::ContentBlockStart {
            index,
            content_block: start(),
        }));
    }
}

/// dsh `StreamChunk` → rebon `StreamEvent`s (the request-direction mapping table).
fn translate_chunk(
    state: &mut TranslateState,
    chunk: &serde_json::Value,
    out: &mut VecDeque<ModelResult<StreamEvent>>,
) {
    let kind = chunk.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let index = chunk.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
    ensure_started(state, out);
    match kind {
        "block-start" => {
            match chunk.get("blockType").and_then(|b| b.as_str()) {
                Some("text") => {
                    ensure_block(state, out, index, || ContentBlockStart::Text {
                        text: String::new(),
                    });
                }
                Some("reasoning") => {
                    ensure_block(state, out, index, || ContentBlockStart::Thinking {
                        thinking: String::new(),
                        data: None,
                    });
                }
                Some("tool-call") => {
                    // id + name arrive with the first tool-call-delta; the
                    // rebon start event waits for them.
                    state.blocks.entry(index).or_insert(BlockState::PendingTool);
                }
                other => {
                    tracing::debug!(?other, "ignoring unknown dsh block type");
                }
            }
        }
        "text-delta" => {
            state.saw_content = true;
            ensure_block(state, out, index, || ContentBlockStart::Text {
                text: String::new(),
            });
            let text = chunk.get("text").and_then(|t| t.as_str()).unwrap_or("");
            out.push_back(Ok(StreamEvent::ContentBlockDelta {
                index,
                delta: ContentBlockDelta::TextDelta {
                    text: text.to_string(),
                },
            }));
        }
        "reasoning-delta" => {
            state.saw_content = true;
            ensure_block(state, out, index, || ContentBlockStart::Thinking {
                thinking: String::new(),
                data: None,
            });
            let text = chunk.get("text").and_then(|t| t.as_str()).unwrap_or("");
            out.push_back(Ok(StreamEvent::ContentBlockDelta {
                index,
                delta: ContentBlockDelta::ThinkingDelta {
                    thinking: text.to_string(),
                },
            }));
        }
        "tool-call-delta" => {
            state.saw_content = true;
            let id = chunk.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let name = chunk.get("name").and_then(|n| n.as_str()).unwrap_or("");
            ensure_block(state, out, index, || ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
            });
            if let Some(delta) = chunk
                .get("argumentsDelta")
                .and_then(|d| d.as_str())
                .filter(|d| !d.is_empty())
            {
                out.push_back(Ok(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::InputJsonDelta {
                        partial_json: delta.to_string(),
                    },
                }));
            }
        }
        "block-end" => {
            if state.blocks.get(&index) == Some(&BlockState::Started) {
                out.push_back(Ok(StreamEvent::ContentBlockStop { index }));
            }
        }
        "usage" => {
            if let Some(usage) = chunk.get("usage") {
                state.usage = parse_dsh_usage(usage);
            }
        }
        "finish" => {
            // dsh FinishReason is an object (`{kind: 'stop'}`, error kind
            // carrying `failure: {message, code}`); the host-side aborted
            // synthesis and older fixtures send the kind as a bare string.
            let reason_value = chunk.get("reason");
            let reason = reason_value
                .and_then(|r| r.as_str())
                .or_else(|| {
                    reason_value
                        .and_then(|r| r.get("kind"))
                        .and_then(|k| k.as_str())
                })
                .unwrap_or("");
            match reason {
                "error" => {
                    // dsh's in-band provider-error channel.
                    let failure = reason_value
                        .and_then(|r| r.get("failure"))
                        .or_else(|| chunk.get("error"));
                    let message = failure
                        .and_then(|f| f.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("adapter reported an in-band provider error");
                    let code = failure.and_then(|f| f.get("code")).and_then(|c| c.as_str());
                    let rendered = match code {
                        Some(code) => format!("[{code}] {message}"),
                        None => message.to_string(),
                    };
                    if code == Some("EMPTY_RESPONSE") {
                        // dsh classifies the degenerate empty completion as
                        // safe to repeat; keep it retryable on this side.
                        out.push_back(Err(ModelError::transient(rendered)));
                    } else {
                        out.push_back(Err(ModelError::Permanent(rendered)));
                    }
                }
                "aborted" => {
                    out.push_back(Err(ModelError::Cancelled));
                }
                _ => {
                    if !state.saw_content {
                        // dsh contract: an empty completion is a retryable
                        // error, never a silent success.
                        out.push_back(Err(ModelError::transient(
                            "EMPTY_RESPONSE: plugin adapter finished without any content",
                        )));
                    } else {
                        out.push_back(Ok(StreamEvent::MessageDelta {
                            delta: MessageDeltaFields {
                                stop_reason: Some(map_finish_reason(reason)),
                                usage: state.usage.clone(),
                            },
                        }));
                        out.push_back(Ok(StreamEvent::MessageStop));
                    }
                }
            }
            state.finished = true;
        }
        other => {
            tracing::debug!(?other, "ignoring unknown dsh stream chunk type");
        }
    }
}

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool-calls" => StopReason::ToolUse,
        "max-tokens" => StopReason::MaxTokens,
        other => StopReason::Other(other.to_string()),
    }
}

/// dsh usage shape → rebon [`Usage`]. Field aliases are deliberately
/// liberal (dsh camelCase plus common provider spellings); exact
/// calibration against real adapters is deliberately empirical.
fn parse_dsh_usage(value: &serde_json::Value) -> Usage {
    let read = |names: &[&str]| -> u32 {
        names
            .iter()
            .find_map(|name| value.get(*name).and_then(|v| v.as_u64()))
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
            .unwrap_or(0)
    };
    let mut usage = Usage {
        input_tokens: read(&["inputTokens", "promptTokens", "input_tokens"]),
        output_tokens: read(&["outputTokens", "completionTokens", "output_tokens"]),
        cache_read_input_tokens: read(&[
            "cacheReadTokens",
            "cachedTokens",
            "cache_read_input_tokens",
        ]),
        ..Usage::default()
    };
    usage.reasoning_tokens = read(&["reasoningTokens", "reasoning_tokens"]);
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{
        ReasoningEffort, ThinkingBlock, ThinkingConfig, Tool, ToolResultBlock, ToolResultContent,
        ToolUseBlock,
    };

    #[test]
    fn request_translates_to_the_dsh_generate_shape() {
        let mut request = CreateMessageRequest::simple("deepseek-v4-pro", "你好");
        request.system = Some("回答要简短".into());
        request.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "先查天气".into(),
                    ..Default::default()
                }),
                ContentBlock::ToolUse(ToolUseBlock {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({ "city": "北京" }),
                }),
            ],
        });
        request.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(ToolResultBlock {
                tool_use_id: "call_1".into(),
                content: ToolResultContent::text("晴，28°C"),
                is_error: false,
            })],
        });
        request.tools = vec![Tool {
            name: "get_weather".into(),
            description: "查询天气".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        request.stop_sequences = vec!["END".into()];
        request.reasoning_effort = Some(ReasoningEffort::Max);

        let payload = dsh_generate_payload("deepseek-official", &request);
        assert_eq!(payload["provider"], "deepseek-official");
        assert_eq!(payload["model"], "deepseek-v4-pro");
        assert_eq!(payload["system"], "回答要简短");
        assert_eq!(payload["stop"], serde_json::json!(["END"]));
        assert_eq!(payload["reasoningEffort"], "max");
        assert_eq!(payload["tools"][0]["parameters"]["type"], "object");

        let messages = payload["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        let assistant = &messages[1]["content"];
        assert_eq!(assistant[0]["type"], "reasoning");
        assert_eq!(assistant[0]["text"], "先查天气");
        assert_eq!(assistant[1]["type"], "tool-call");
        // dsh carries tool arguments as the raw JSON string.
        assert_eq!(assistant[1]["arguments"], r#"{"city":"北京"}"#);
        let result = &messages[2]["content"][0];
        assert_eq!(result["type"], "tool-result");
        assert_eq!(result["toolCallId"], "call_1");
        assert_eq!(result["content"][0]["text"], "晴，28°C");
        assert!(result.get("isError").is_none(), "{result}");
    }

    #[test]
    fn reasoning_effort_folds_thinking_config_and_effort_tiers() {
        let mut request = CreateMessageRequest::simple("m", "hi");
        assert_eq!(dsh_reasoning_effort(&request), None);

        request.thinking = Some(ThinkingConfig::Disabled);
        request.reasoning_effort = Some(ReasoningEffort::High);
        // An explicitly disabled thinking config wins over any effort tier.
        assert_eq!(dsh_reasoning_effort(&request), Some("off"));

        request.thinking = Some(ThinkingConfig::Enabled { budget_tokens: 1 });
        assert_eq!(dsh_reasoning_effort(&request), Some("high"));
        request.reasoning_effort = Some(ReasoningEffort::Low);
        assert_eq!(dsh_reasoning_effort(&request), Some("off"));
        request.reasoning_effort = Some(ReasoningEffort::XHigh);
        assert_eq!(dsh_reasoning_effort(&request), Some("max"));
    }

    #[test]
    fn transient_context_rides_the_last_user_message() {
        let mut request = CreateMessageRequest::simple("m", "问题");
        request.transient_context = Some("动态上下文".into());
        let payload = dsh_generate_payload("p", &request);
        let content = payload["messages"][0]["content"]
            .as_array()
            .expect("blocks");
        assert_eq!(content.len(), 2, "{content:?}");
        assert!(
            content[1]["text"].as_str().unwrap().contains("动态上下文"),
            "{content:?}"
        );
    }

    fn drain(
        state: &mut TranslateState,
        chunks: &[serde_json::Value],
    ) -> Vec<ModelResult<StreamEvent>> {
        let mut out = VecDeque::new();
        for chunk in chunks {
            translate_chunk(state, chunk, &mut out);
        }
        out.into_iter().collect()
    }

    #[test]
    fn full_turn_translates_to_the_expected_event_sequence() {
        let mut state = TranslateState::new("m1".into(), "fake-large".into());
        let events = drain(
            &mut state,
            &[
                serde_json::json!({"type": "block-start", "index": 0, "blockType": "reasoning"}),
                serde_json::json!({"type": "reasoning-delta", "index": 0, "text": "思考中"}),
                serde_json::json!({"type": "block-end", "index": 0}),
                serde_json::json!({"type": "block-start", "index": 1, "blockType": "text"}),
                serde_json::json!({"type": "text-delta", "index": 1, "text": "你好"}),
                serde_json::json!({"type": "block-end", "index": 1}),
                serde_json::json!({"type": "block-start", "index": 2, "blockType": "tool-call"}),
                serde_json::json!({"type": "tool-call-delta", "index": 2, "id": "call_1", "name": "get_weather", "argumentsDelta": "{\"city\":"}),
                serde_json::json!({"type": "tool-call-delta", "index": 2, "id": "call_1", "argumentsDelta": "\"北京\"}"}),
                serde_json::json!({"type": "block-end", "index": 2}),
                serde_json::json!({"type": "usage", "usage": {"inputTokens": 12, "outputTokens": 34, "reasoningTokens": 3}}),
                serde_json::json!({"type": "finish", "reason": "tool-calls"}),
            ],
        );

        let ok: Vec<&StreamEvent> = events.iter().map(|e| e.as_ref().expect("all ok")).collect();
        assert!(matches!(ok[0], StreamEvent::MessageStart { model, .. } if model == "fake-large"));
        assert!(matches!(
            ok[1],
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Thinking { .. }
            }
        ));
        assert!(matches!(
            ok[2],
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::ThinkingDelta { thinking } } if thinking == "思考中"
        ));
        assert!(matches!(ok[3], StreamEvent::ContentBlockStop { index: 0 }));
        assert!(matches!(
            ok[4],
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockStart::Text { .. }
            }
        ));
        assert!(matches!(
            ok[5],
            StreamEvent::ContentBlockDelta { index: 1, delta: ContentBlockDelta::TextDelta { text } } if text == "你好"
        ));
        assert!(matches!(ok[6], StreamEvent::ContentBlockStop { index: 1 }));
        assert!(matches!(
            ok[7],
            StreamEvent::ContentBlockStart { index: 2, content_block: ContentBlockStart::ToolUse { id, name } }
                if id == "call_1" && name == "get_weather"
        ));
        assert!(matches!(
            ok[8],
            StreamEvent::ContentBlockDelta {
                index: 2,
                delta: ContentBlockDelta::InputJsonDelta { .. }
            }
        ));
        assert!(matches!(
            ok[9],
            StreamEvent::ContentBlockDelta {
                index: 2,
                delta: ContentBlockDelta::InputJsonDelta { .. }
            }
        ));
        assert!(matches!(ok[10], StreamEvent::ContentBlockStop { index: 2 }));
        assert!(matches!(
            ok[11],
            StreamEvent::MessageDelta { delta: MessageDeltaFields { stop_reason: Some(StopReason::ToolUse), usage } }
                if usage.input_tokens == 12 && usage.output_tokens == 34 && usage.reasoning_tokens == 3
        ));
        assert!(matches!(ok[12], StreamEvent::MessageStop));
        assert!(state.finished);
    }

    #[test]
    fn empty_completion_is_a_retryable_error() {
        let mut state = TranslateState::new("m1".into(), "fake".into());
        let events = drain(
            &mut state,
            &[serde_json::json!({"type": "finish", "reason": "stop"})],
        );
        // MessageStart is synthesized, then the EMPTY_RESPONSE error.
        assert_eq!(events.len(), 2);
        let err = events[1].as_ref().expect_err("empty turn errors");
        assert!(err.to_string().contains("EMPTY_RESPONSE"), "{err}");
        assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
    }

    #[test]
    fn aborted_and_error_finishes_map_to_model_errors() {
        let mut state = TranslateState::new("m1".into(), "fake".into());
        let events = drain(
            &mut state,
            &[
                serde_json::json!({"type": "text-delta", "index": 0, "text": "部分"}),
                serde_json::json!({"type": "finish", "reason": "aborted"}),
            ],
        );
        assert!(matches!(
            events.last().unwrap().as_ref().expect_err("aborted"),
            ModelError::Cancelled
        ));

        let mut state = TranslateState::new("m2".into(), "fake".into());
        let events = drain(
            &mut state,
            &[
                serde_json::json!({"type": "text-delta", "index": 0, "text": "部分"}),
                serde_json::json!({"type": "finish", "reason": "error", "error": {"message": "上游 500"}}),
            ],
        );
        let err = events.last().unwrap().as_ref().expect_err("in-band error");
        assert!(err.to_string().contains("上游 500"), "{err}");
    }

    #[test]
    fn object_shaped_finish_reasons_translate_like_dsh_emits_them() {
        // Real dsh translate.ts emits `reason: {kind: …}` objects.
        let mut state = TranslateState::new("m1".into(), "fake".into());
        let events = drain(
            &mut state,
            &[
                serde_json::json!({"type": "text-delta", "index": 0, "text": "好"}),
                serde_json::json!({"type": "finish", "reason": {"kind": "stop"}}),
            ],
        );
        assert!(matches!(
            events[events.len() - 2].as_ref().unwrap(),
            StreamEvent::MessageDelta {
                delta: MessageDeltaFields {
                    stop_reason: Some(StopReason::EndTurn),
                    ..
                }
            }
        ));
        assert!(matches!(
            events.last().unwrap().as_ref().unwrap(),
            StreamEvent::MessageStop
        ));

        // In-band error kind carries `failure: {message, code}`.
        let mut state = TranslateState::new("m2".into(), "fake".into());
        let events = drain(
            &mut state,
            &[
                serde_json::json!({"type": "text-delta", "index": 0, "text": "部分"}),
                serde_json::json!({"type": "finish", "reason": {"kind": "error",
                    "failure": {"message": "太长了", "code": "CONTEXT_WINDOW_EXCEEDED"}}}),
            ],
        );
        let err = events.last().unwrap().as_ref().expect_err("in-band error");
        assert!(matches!(err, ModelError::Permanent(_)), "{err:?}");
        assert!(
            err.to_string().contains("[CONTEXT_WINDOW_EXCEEDED] 太长了"),
            "{err}"
        );

        // dsh's own empty-completion classification stays retryable here.
        let mut state = TranslateState::new("m3".into(), "fake".into());
        let events = drain(
            &mut state,
            &[
                serde_json::json!({"type": "finish", "reason": {"kind": "error",
                "failure": {"message": "no content", "code": "EMPTY_RESPONSE"}}}),
            ],
        );
        let err = events
            .last()
            .unwrap()
            .as_ref()
            .expect_err("empty completion");
        assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
    }

    #[test]
    fn deltas_without_block_start_synthesize_starts() {
        let mut state = TranslateState::new("m1".into(), "fake".into());
        let events = drain(
            &mut state,
            &[serde_json::json!({"type": "text-delta", "index": 0, "text": "裸增量"})],
        );
        assert!(matches!(
            events[1].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockStart::Text { .. }
            }
        ));
        assert!(matches!(
            events[2].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { .. }
        ));
    }
}
