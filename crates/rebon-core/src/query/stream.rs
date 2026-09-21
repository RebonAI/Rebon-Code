use super::*;

pub(super) async fn publish_session_update(
    publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    session_id: &str,
    update: SessionUpdate,
) {
    let Some(publisher) = publisher else {
        return;
    };
    publisher.publish_to(&session_id.to_string(), update).await;
}

pub(super) async fn forward_tool_dispatch_event(
    publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    session_id: &str,
    event: &QueryEvent,
) {
    match event {
        QueryEvent::ToolDispatchStart {
            tool_use_id,
            name,
            input,
        } => {
            let raw_map = input.as_object().cloned();
            let title = raw_map
                .as_ref()
                .map(|input| build_tool_call_title(name, input))
                .unwrap_or_else(|| name.clone());
            let raw_input = raw_map.map(|m| m.into_iter().collect::<HashMap<String, Value>>());
            publish_session_update(
                publisher,
                session_id,
                SessionUpdate::ToolCall {
                    tool_call_id: tool_use_id.clone(),
                    title,
                    kind: tool_name_to_kind(name),
                    status: ToolCallStatus::Pending,
                    content: None,
                    locations: None,
                    raw_input,
                    raw_output: None,
                },
            )
            .await;
            publish_session_update(
                publisher,
                session_id,
                SessionUpdate::ToolCallUpdate {
                    tool_call_id: tool_use_id.clone(),
                    status: Some(ToolCallStatus::InProgress),
                    title: None,
                    content: None,
                    locations: None,
                    raw_output: None,
                },
            )
            .await;
        }
        QueryEvent::ToolAutoModeAllowed {
            tool_use_id,
            source,
        } => {
            publish_session_update(
                publisher,
                session_id,
                SessionUpdate::ToolCallAutoModeAllowed {
                    tool_call_id: tool_use_id.clone(),
                    source: *source,
                },
            )
            .await;
        }
        QueryEvent::ToolDispatchProgress {
            tool_use_id,
            progress,
            ..
        } => {
            let content = rebon_session_state::tool_output::tool_progress_update_content(progress);
            publish_session_update(
                publisher,
                session_id,
                SessionUpdate::ToolCallUpdate {
                    tool_call_id: tool_use_id.clone(),
                    status: Some(ToolCallStatus::InProgress),
                    title: None,
                    content,
                    locations: None,
                    raw_output: progress.payload.clone().and_then(|v| {
                        v.as_object().map(|m| {
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<HashMap<String, Value>>()
                        })
                    }),
                },
            )
            .await;
        }
        QueryEvent::ToolDispatchResult {
            tool_use_id,
            name,
            outcome,
            error_presentation,
        } => match outcome {
            Ok(value) => {
                let title = if name == "Skill" {
                    value
                        .get("skill")
                        .and_then(Value::as_str)
                        .map(|skill| format!("Loaded /{skill}"))
                } else {
                    None
                };
                publish_session_update(
                    publisher,
                    session_id,
                    SessionUpdate::ToolCallUpdate {
                        tool_call_id: tool_use_id.clone(),
                        status: Some(ToolCallStatus::Completed),
                        title,
                        content: tool_result_update_content(value),
                        locations: extract_locations(value),
                        raw_output: value.as_object().map(|m| {
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<HashMap<String, Value>>()
                        }),
                    },
                )
                .await;
            }
            Err(err) => {
                publish_session_update(
                    publisher,
                    session_id,
                    SessionUpdate::ToolCallUpdate {
                        tool_call_id: tool_use_id.clone(),
                        status: Some(ToolCallStatus::Failed),
                        title: None,
                        content: Some(vec![ToolCallContent::Content(
                            rebon_types::RegularContent {
                                content: AcpContentBlock::Text(TextContent {
                                    text: error_presentation
                                        .as_ref()
                                        .map(|presentation| presentation.display_message.clone())
                                        .unwrap_or_else(|| err.clone()),
                                    annotations: None,
                                }),
                            },
                        )]),
                        locations: None,
                        raw_output: error_presentation.as_ref().map(|presentation| {
                            HashMap::from([
                                (
                                    "errorCode".to_string(),
                                    Value::String(presentation.code.clone()),
                                ),
                                (
                                    "displayMessage".to_string(),
                                    Value::String(presentation.display_message.clone()),
                                ),
                            ])
                        }),
                    },
                )
                .await;
            }
        },
        _ => {}
    }
}

#[derive(Default)]
pub(super) struct StreamForwardState {
    pub(super) image_generation_blocks: HashMap<usize, String>,
    pub(super) tool_use_blocks: HashMap<usize, (String, String)>,
    pub(super) tool_use_input_buffers: HashMap<usize, String>,
    /// Every client/server tool card announced to the session from raw
    /// stream events, keyed by `tool_call_id` → tool name. Unlike
    /// `tool_use_blocks` (keyed by block INDEX, so a retried stream
    /// attempt silently overwrites the abandoned attempt's entries),
    /// this map keeps every id ever announced, which is what
    /// [`publish_orphaned_tool_card_failures`] reconciles against the
    /// finished message at iteration end. Drained there each iteration.
    pub(super) announced_tool_card_ids: HashMap<String, String>,
    pub(super) input_tokens: u32,
    pub(super) streamed_output_chars: usize,
    pub(super) published_output_tokens: u32,
}

impl StreamForwardState {
    pub(super) fn usage_snapshot(&self) -> (u32, u32) {
        (self.input_tokens, self.published_output_tokens)
    }

    fn merge_usage(&mut self, usage: &Usage) -> bool {
        let before = self.usage_snapshot();
        if usage.input_tokens > 0 {
            self.input_tokens = usage.input_tokens;
        }
        if usage.output_tokens > 0 {
            self.published_output_tokens = usage.output_tokens;
            self.streamed_output_chars = (usage.output_tokens as usize).saturating_mul(4);
        }
        self.usage_snapshot() != before
    }

    fn record_streamed_output(&mut self, text_len: usize) -> bool {
        const MIN_TOKEN_DELTA: u32 = 50;
        self.streamed_output_chars = self.streamed_output_chars.saturating_add(text_len);
        let output_tokens = approximate_streamed_output_tokens(self.streamed_output_chars);
        if output_tokens >= self.published_output_tokens + MIN_TOKEN_DELTA {
            self.published_output_tokens = output_tokens;
            true
        } else {
            false
        }
    }

    /// Force-publish the approximate token count if it exceeds what was
    /// last published. Called at stream end to cover short responses
    /// whose total never crossed the throttle threshold.
    pub(super) fn flush_streamed_usage(&mut self) -> bool {
        let output_tokens = approximate_streamed_output_tokens(self.streamed_output_chars);
        if output_tokens > self.published_output_tokens {
            self.published_output_tokens = output_tokens;
            true
        } else {
            false
        }
    }
}

pub(super) fn json_object_to_hash_map(value: &Value) -> Option<HashMap<String, Value>> {
    value
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

pub(super) fn text_tool_call_content(text: impl Into<String>) -> Vec<ToolCallContent> {
    vec![ToolCallContent::Content(rebon_types::RegularContent {
        content: AcpContentBlock::Text(TextContent {
            text: text.into(),
            annotations: None,
        }),
    })]
}

pub(super) fn approximate_streamed_output_tokens(chars: usize) -> u32 {
    (chars / 4) as u32
}

pub(super) async fn publish_token_usage_snapshot(
    publisher: &Arc<dyn SessionUpdatePublisher>,
    session_id: &str,
    input_tokens: u32,
    output_tokens: u32,
) {
    if input_tokens == 0 && output_tokens == 0 {
        return;
    }
    publisher
        .publish_to(
            &session_id.to_string(),
            SessionUpdate::TokenUsage {
                input_tokens,
                output_tokens,
            },
        )
        .await;
}

pub(super) async fn publish_generated_image_completion(
    state: &StreamForwardState,
    publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    session_id: &str,
    message: &AssistantMessage,
) {
    let Some(publisher) = publisher else {
        return;
    };

    for block in &message.content {
        let ApiContentBlock::GeneratedImage(image) = block else {
            continue;
        };
        let mut raw_output = HashMap::new();
        raw_output.insert("status".to_string(), Value::String("completed".to_string()));
        if let Some(prompt) = &image.revised_prompt {
            raw_output.insert("revised_prompt".to_string(), Value::String(prompt.clone()));
        }

        let locations = image.saved_path.as_ref().map(|path| {
            vec![ToolCallLocation {
                path: path.clone(),
                line: None,
            }]
        });
        let content = if locations.is_some() {
            None
        } else {
            Some(text_tool_call_content("Generated image completed"))
        };

        publisher
            .publish_to(
                &session_id.to_string(),
                SessionUpdate::ToolCallUpdate {
                    tool_call_id: image.id.clone(),
                    status: Some(ToolCallStatus::Completed),
                    title: Some("Image generated".to_string()),
                    content,
                    locations,
                    raw_output: Some(raw_output),
                },
            )
            .await;
    }

    for id in state.image_generation_blocks.values() {
        if message
            .content
            .iter()
            .any(|block| matches!(block, ApiContentBlock::GeneratedImage(image) if image.id == *id))
        {
            continue;
        }
        publisher
            .publish_to(
                &session_id.to_string(),
                SessionUpdate::ToolCallUpdate {
                    tool_call_id: id.clone(),
                    status: Some(ToolCallStatus::Completed),
                    title: Some("Image generation completed".to_string()),
                    content: None,
                    locations: None,
                    raw_output: None,
                },
            )
            .await;
    }
}

/// Close tool cards that were announced from raw stream events but whose
/// blocks are absent from the finished assistant message.
///
/// The WS transport (`drive_ws_turn`) recovers from a mid-stream break by
/// re-driving the SAME turn into the SAME event stream. Any tool-use block
/// the abandoned attempt had already started was announced to the session
/// as a Pending/InProgress card, but the retried response generates fresh
/// tool ids: the accumulator overwrites the stale block by index, the
/// executor never dispatches it, and nothing ever publishes a terminal
/// status for the card. A permanently-InProgress card stalls the inline
/// sealed-prefix flush at the front of the streaming overlay — the whole
/// rest of the turn piles up in the live region (top-clipped, unreachable
/// by scrollback) until the turn finalizes. Reconcile at iteration end:
/// every announced id missing from the finished message gets a Failed
/// terminal update so downstream state machines can seal past it.
pub(super) async fn publish_orphaned_tool_card_failures(
    state: &mut StreamForwardState,
    publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    session_id: &str,
    message: &AssistantMessage,
) {
    if state.announced_tool_card_ids.is_empty() {
        return;
    }
    let finished_ids: std::collections::HashSet<&str> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ApiContentBlock::ToolUse(tool_use) => Some(tool_use.id.as_str()),
            ApiContentBlock::ServerToolUse(server_tool_use) => Some(server_tool_use.id.as_str()),
            _ => None,
        })
        .collect();
    // Ids that made it into the finished message are owned by the dispatch
    // pipeline (or the server-tool result events) from here on; drain the
    // whole map so the next iteration starts clean either way.
    let orphans: Vec<(String, String)> = state
        .announced_tool_card_ids
        .drain()
        .filter(|(id, _)| !finished_ids.contains(id.as_str()))
        .collect();
    let Some(publisher) = publisher else {
        return;
    };
    for (tool_call_id, name) in orphans {
        tracing::warn!(
            tool_call_id = %tool_call_id,
            tool = %name,
            "closing orphaned tool card: stream attempt was abandoned before the call completed"
        );
        publisher
            .publish_to(
                &session_id.to_string(),
                SessionUpdate::ToolCallUpdate {
                    tool_call_id,
                    status: Some(ToolCallStatus::Failed),
                    title: None,
                    content: Some(text_tool_call_content(
                        "Interrupted: the model stream was retried before this tool call completed.",
                    )),
                    locations: None,
                    raw_output: None,
                },
            )
            .await;
    }
}

pub(super) async fn forward_stream_event(
    state: &mut StreamForwardState,
    publisher: &Option<Arc<dyn SessionUpdatePublisher>>,
    session_id: &str,
    event: &StreamEvent,
) {
    let Some(publisher) = publisher else {
        return;
    };
    // Any content block other than text/thinking starting means the
    // current extended-thinking stream (if any) is finished. The
    // `Text` arm below already closes thinking on its own, but
    // reasoning models stream `[Thinking, ToolUse, Thinking, ToolUse,
    // …]` with no text in between — without also closing thinking when
    // a tool/image/search block starts, every `Thinking` block stays
    // `is_streaming` forever. That stalls the inline sealed-prefix
    // flush at the very first overlay block (the forward walk breaks on
    // an open thinking block), so the overlay grows unbounded and
    // nothing drains into scrollback. Emitting here is idempotent: a
    // redundant `ThinkingEnd` with no open thinking block is a no-op.
    if let StreamEvent::ContentBlockStart { content_block, .. } = event {
        if !matches!(
            content_block,
            rebon_api::ContentBlockStart::Text { .. }
                | rebon_api::ContentBlockStart::Thinking { .. }
        ) {
            publisher
                .publish_to(&session_id.to_string(), SessionUpdate::ThinkingEnd)
                .await;
        }
    }
    match event {
        StreamEvent::MessageStart { usage, .. } => {
            // Forward the token usage snapshot so the TUI can display
            // the running token count in real time.
            if state.merge_usage(usage) {
                let (input_tokens, output_tokens) = state.usage_snapshot();
                publish_token_usage_snapshot(publisher, session_id, input_tokens, output_tokens)
                    .await;
            }
        }
        StreamEvent::ContentBlockStart {
            content_block: rebon_api::ContentBlockStart::Text { .. },
            ..
        } => {
            // A text block starting means thinking is done.
            publisher
                .publish_to(&session_id.to_string(), SessionUpdate::ThinkingEnd)
                .await;
        }
        StreamEvent::ContentBlockStart {
            index,
            content_block: rebon_api::ContentBlockStart::ToolUse { id, name },
        } => {
            state
                .tool_use_blocks
                .insert(*index, (id.clone(), name.clone()));
            state
                .announced_tool_card_ids
                .insert(id.clone(), name.clone());
            state.tool_use_input_buffers.remove(index);
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ToolCall {
                        tool_call_id: id.clone(),
                        title: name.clone(),
                        kind: tool_name_to_kind(name),
                        status: ToolCallStatus::Pending,
                        content: None,
                        locations: None,
                        raw_input: None,
                        raw_output: None,
                    },
                )
                .await;
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ToolCallUpdate {
                        tool_call_id: id.clone(),
                        status: Some(ToolCallStatus::InProgress),
                        title: None,
                        content: None,
                        locations: None,
                        raw_output: None,
                    },
                )
                .await;
        }
        StreamEvent::ContentBlockStart {
            content_block:
                rebon_api::ContentBlockStart::ServerToolUse {
                    id, name, input, ..
                },
            ..
        } => {
            // Server-side tool use (e.g. web search) is executed by
            // the provider, not dispatched client-side. Emit a normal
            // ToolCall start first; a ToolCallUpdate alone is ignored
            // by the local streaming overlay when no card exists yet.
            state
                .announced_tool_card_ids
                .insert(id.clone(), name.clone());
            let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let title = if query.is_empty() {
                name.clone()
            } else {
                format!("{name} \"{query}\"")
            };
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ToolCall {
                        tool_call_id: id.clone(),
                        title,
                        kind: tool_name_to_kind(name),
                        status: ToolCallStatus::Pending,
                        content: None,
                        locations: None,
                        raw_input: json_object_to_hash_map(input),
                        raw_output: None,
                    },
                )
                .await;
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ToolCallUpdate {
                        tool_call_id: id.clone(),
                        status: Some(ToolCallStatus::InProgress),
                        title: None,
                        content: None,
                        locations: None,
                        raw_output: None,
                    },
                )
                .await;
        }
        StreamEvent::ContentBlockStart {
            index,
            content_block: rebon_api::ContentBlockStart::ImageGeneration { id, .. },
        } => {
            state.image_generation_blocks.insert(*index, id.clone());
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ToolCall {
                        tool_call_id: id.clone(),
                        title: "ImageGeneration".to_string(),
                        kind: ToolKind::Other,
                        status: ToolCallStatus::Pending,
                        content: None,
                        locations: None,
                        raw_input: None,
                        raw_output: None,
                    },
                )
                .await;
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ToolCallUpdate {
                        tool_call_id: id.clone(),
                        status: Some(ToolCallStatus::InProgress),
                        title: None,
                        content: None,
                        locations: None,
                        raw_output: None,
                    },
                )
                .await;
        }
        StreamEvent::ContentBlockStart {
            content_block:
                rebon_api::ContentBlockStart::WebSearchResult {
                    tool_use_id,
                    content,
                },
            ..
        } => {
            let result_count = content.as_array().map(|a| a.len()).unwrap_or(0);
            let summary = format!("Found {result_count} results");
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ToolCallUpdate {
                        tool_call_id: tool_use_id.clone(),
                        status: Some(ToolCallStatus::Completed),
                        title: Some(summary),
                        content: None,
                        locations: None,
                        raw_output: None,
                    },
                )
                .await;
        }
        StreamEvent::ContentBlockDelta { index, delta } => {
            forward_content_block_delta(state, publisher, session_id, index, delta).await;
        }
        StreamEvent::ContentBlockStop { index } => {
            if let Some((id, name)) = state.tool_use_blocks.remove(index) {
                let input = state
                    .tool_use_input_buffers
                    .remove(index)
                    .and_then(|buffer| serde_json::from_str::<Value>(&buffer).ok())
                    .and_then(|value| value.as_object().cloned());
                let title = input
                    .as_ref()
                    .map(|input| build_tool_call_title(&name, input));
                publisher
                    .publish_to(
                        &session_id.to_string(),
                        SessionUpdate::ToolCallUpdate {
                            tool_call_id: id,
                            status: Some(ToolCallStatus::InProgress),
                            title,
                            content: None,
                            locations: None,
                            raw_output: None,
                        },
                    )
                    .await;
            }
            if let Some(id) = state.image_generation_blocks.get(index) {
                publisher
                    .publish_to(
                        &session_id.to_string(),
                        SessionUpdate::ToolCallUpdate {
                            tool_call_id: id.clone(),
                            status: Some(ToolCallStatus::InProgress),
                            title: None,
                            content: Some(text_tool_call_content("Finalizing generated image")),
                            locations: None,
                            raw_output: None,
                        },
                    )
                    .await;
            }
        }
        StreamEvent::MessageDelta { delta } => {
            if state.merge_usage(&delta.usage) {
                let (input_tokens, output_tokens) = state.usage_snapshot();
                publish_token_usage_snapshot(publisher, session_id, input_tokens, output_tokens)
                    .await;
            }
        }
        StreamEvent::MessageStop => {
            // A message can END on a thinking block — max_tokens hit
            // mid-reasoning, pause_turn, or a provider that closes the
            // turn without a trailing text block. No later block start
            // will ever fire to close it, and a thinking block stuck
            // `is_streaming` stalls the inline sealed-prefix flush for
            // the rest of the turn (see the block-start comment above).
            // Idempotent: with no open thinking block this is a no-op.
            publisher
                .publish_to(&session_id.to_string(), SessionUpdate::ThinkingEnd)
                .await;
        }
        _ => {}
    }
}

#[cfg(test)]
mod thinking_end_on_block_start_tests {
    use super::*;
    use rebon_agent_core::MemorySessionUpdatePublisher;

    fn block_start(content_block: rebon_api::ContentBlockStart) -> StreamEvent {
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block,
        }
    }

    async fn updates_for(event: &StreamEvent) -> Vec<SessionUpdate> {
        let recorder = Arc::new(MemorySessionUpdatePublisher::new());
        let publisher: Option<Arc<dyn SessionUpdatePublisher>> = Some(recorder.clone());
        let mut state = StreamForwardState::default();
        forward_stream_event(&mut state, &publisher, "sess-1", event).await;
        recorder
            .snapshot()
            .into_iter()
            .map(|params| params.update)
            .collect()
    }

    fn has_thinking_end(updates: &[SessionUpdate]) -> bool {
        updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ThinkingEnd))
    }

    // Reasoning models stream `[Thinking, ToolUse, Thinking, ToolUse, …]`
    // with no text between tool calls. A tool block starting must close
    // the open thinking stream — otherwise every `Thinking` block stays
    // `is_streaming` forever, the inline sealed-prefix forward walk
    // breaks on the first open block, and the overlay never drains into
    // scrollback (content gets clipped off the top and lost).
    #[tokio::test]
    async fn tool_failure_uses_concise_display_message_for_session_updates() {
        let recorder = Arc::new(MemorySessionUpdatePublisher::new());
        let publisher: Option<Arc<dyn SessionUpdatePublisher>> = Some(recorder.clone());
        let event = QueryEvent::ToolDispatchResult {
            tool_use_id: "toolu-send".into(),
            name: "SendMessage".into(),
            outcome: Err("full model-facing recovery instructions".into()),
            error_presentation: Some(ToolErrorPresentation::new(
                "agent_closed",
                "Agent is no longer available.",
                "full model-facing recovery instructions",
            )),
        };

        forward_tool_dispatch_event(&publisher, "sess-1", &event).await;

        let updates = recorder.snapshot();
        let SessionUpdate::ToolCallUpdate {
            status,
            content,
            raw_output,
            ..
        } = &updates[0].update
        else {
            panic!("expected tool call update");
        };
        assert_eq!(*status, Some(ToolCallStatus::Failed));
        let Some(ToolCallContent::Content(rebon_types::RegularContent {
            content: AcpContentBlock::Text(text),
        })) = content.as_ref().and_then(|content| content.first())
        else {
            panic!("expected text tool error content");
        };
        assert_eq!(text.text, "Agent is no longer available.");
        assert_eq!(
            raw_output
                .as_ref()
                .and_then(|output| output.get("errorCode"))
                .and_then(Value::as_str),
            Some("agent_closed")
        );
    }

    #[tokio::test]
    async fn tool_use_block_start_publishes_thinking_end() {
        let updates = updates_for(&block_start(rebon_api::ContentBlockStart::ToolUse {
            id: "t1".to_string(),
            name: "Read".to_string(),
        }))
        .await;
        assert!(
            has_thinking_end(&updates),
            "ToolUse block start must publish ThinkingEnd to close an open reasoning block; got {updates:?}"
        );
    }

    #[tokio::test]
    async fn image_generation_block_start_publishes_thinking_end() {
        let updates = updates_for(&block_start(
            rebon_api::ContentBlockStart::ImageGeneration {
                id: "ig1".to_string(),
                status: None,
            },
        ))
        .await;
        assert!(
            has_thinking_end(&updates),
            "ImageGeneration block start must publish ThinkingEnd; got {updates:?}"
        );
    }

    // Text block start still closes thinking — unchanged behaviour, kept
    // so a future refactor cannot silently drop it.
    #[tokio::test]
    async fn text_block_start_still_publishes_thinking_end() {
        let updates = updates_for(&block_start(rebon_api::ContentBlockStart::Text {
            text: String::new(),
        }))
        .await;
        assert!(
            has_thinking_end(&updates),
            "Text block start must still publish ThinkingEnd; got {updates:?}"
        );
    }

    // A message can end ON a thinking block (max_tokens mid-reasoning,
    // pause_turn). No later block start fires to close it, so MessageStop
    // must publish the ThinkingEnd itself — otherwise the TUI overlay
    // keeps the block `is_streaming` and the inline sealed-prefix flush
    // stalls there for the rest of the turn.
    #[tokio::test]
    async fn message_stop_publishes_thinking_end() {
        let updates = updates_for(&StreamEvent::MessageStop).await;
        assert!(
            has_thinking_end(&updates),
            "MessageStop must publish ThinkingEnd to close a reasoning block the message ended on; got {updates:?}"
        );
    }

    // A thinking block *starting* is the reasoning stream beginning, not
    // ending — publishing ThinkingEnd here would close the very block
    // being opened, so it must NOT fire.
    #[tokio::test]
    async fn thinking_block_start_does_not_publish_thinking_end() {
        let updates = updates_for(&block_start(rebon_api::ContentBlockStart::Thinking {
            thinking: String::new(),
            data: None,
        }))
        .await;
        assert!(
            !has_thinking_end(&updates),
            "Thinking block start must NOT publish ThinkingEnd; got {updates:?}"
        );
    }
}

/// Forward one `ContentBlockDelta` on to the session update stream.
///
/// One arm per delta kind: text and thinking append to their open block, a
/// tool-use partial JSON delta accumulates into the block's input buffer, and
/// the signature delta seals a thinking block.
async fn forward_content_block_delta(
    state: &mut StreamForwardState,
    publisher: &Arc<dyn SessionUpdatePublisher>,
    session_id: &str,
    index: &usize,
    delta: &rebon_api::ContentBlockDelta,
) {
    match delta {
        rebon_api::ContentBlockDelta::TextDelta { text } => {
            tracing::trace!(
                target: "stream_dbg",
                sess = %session_id,
                len = text.len(),
                "engine: publish AgentMessageChunk(text)"
            );
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::AgentMessageChunk {
                        content: AcpContentBlock::Text(TextContent {
                            text: text.clone(),
                            annotations: None,
                        }),
                    },
                )
                .await;
            if state.record_streamed_output(text.len()) {
                let (input_tokens, output_tokens) = state.usage_snapshot();
                publish_token_usage_snapshot(publisher, session_id, input_tokens, output_tokens)
                    .await;
            }
        }
        rebon_api::ContentBlockDelta::ThinkingDelta { thinking } => {
            tracing::trace!(
                target: "stream_dbg",
                sess = %session_id,
                len = thinking.len(),
                "engine: publish ThinkingDelta"
            );
            publisher
                .publish_to(
                    &session_id.to_string(),
                    SessionUpdate::ThinkingDelta {
                        text: thinking.clone(),
                    },
                )
                .await;
            if state.record_streamed_output(thinking.len()) {
                let (input_tokens, output_tokens) = state.usage_snapshot();
                publish_token_usage_snapshot(publisher, session_id, input_tokens, output_tokens)
                    .await;
            }
        }
        rebon_api::ContentBlockDelta::InputJsonDelta { partial_json } => {
            if state.record_streamed_output(partial_json.len()) {
                let (input_tokens, output_tokens) = state.usage_snapshot();
                publish_token_usage_snapshot(publisher, session_id, input_tokens, output_tokens)
                    .await;
            }
            if let Some((id, name)) = state.tool_use_blocks.get(index) {
                let buffer = state.tool_use_input_buffers.entry(*index).or_default();
                buffer.push_str(partial_json);
                let input = serde_json::from_str::<Value>(buffer)
                    .ok()
                    .and_then(|value| value.as_object().cloned());
                let title = input
                    .as_ref()
                    .map(|input| build_tool_call_title(name, input));
                publisher
                    .publish_to(
                        &session_id.to_string(),
                        SessionUpdate::ToolCallUpdate {
                            tool_call_id: id.clone(),
                            status: Some(ToolCallStatus::InProgress),
                            title,
                            content: None,
                            locations: None,
                            raw_output: None,
                        },
                    )
                    .await;
            }
        }
        rebon_api::ContentBlockDelta::ImageDataDelta {
            partial_index,
            revised_prompt,
            ..
        } => {
            if let Some(id) = state.image_generation_blocks.get(index) {
                let content = partial_index.map(|partial| {
                    text_tool_call_content(format!("Generated preview frame {}", partial + 1))
                });
                publisher
                    .publish_to(
                        &session_id.to_string(),
                        SessionUpdate::ToolCallUpdate {
                            tool_call_id: id.clone(),
                            status: Some(ToolCallStatus::InProgress),
                            title: None,
                            content,
                            locations: None,
                            raw_output: revised_prompt.as_ref().map(|prompt| {
                                HashMap::from([(
                                    "revised_prompt".to_string(),
                                    Value::String(prompt.clone()),
                                )])
                            }),
                        },
                    )
                    .await;
            }
        }
        _ => {}
    }
}
