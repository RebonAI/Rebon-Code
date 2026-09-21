//! Translate [`SessionUpdateParams`] from the engine's session
//! update stream into `rebon_tui::Action`s that the transcript
//! reducer can apply.
//!
//! Drives the "read path" of the TUI wiring:
//!
//! ```text
//!    engine / tool runtime
//!        │
//!        │ ChannelSessionUpdatePublisher
//!        ▼
//!    UnboundedReceiver<SessionUpdateParams>
//!        │
//!        │ runner drains via try_recv()
//!        ▼
//!    translate_session_update(&mut AppState, params)   ← this module
//!        │
//!        │ rebon_tui::reducer(&mut state.rebon_tui, action)
//!        ▼
//!    state.rebon_tui (transcript + streaming overlay)
//!        │
//!        │ frame draw
//!        ▼
//!    rebon_tui::render_transcript(...)
//! ```
//!
//! ## Current scope
//!
//! [`SessionUpdate::AgentMessageChunk`] text deltas and ACP tool-call
//! lifecycle updates are translated into `rebon_tui::Action`s.
//! `Plan`, `SlashCommands`, `ConfigOptionUpdate` and
//! `SessionInfoUpdate` write their `AppState` field directly
//! (`plan_entries`, `slash_commands`, `config_options`,
//! `session_title`).
//!
//! The intentional gaps:
//!
//! * **Non-text `AgentMessageChunk` content** (image / audio /
//!   resource / resource link) — the `rebon-tui` transcript model
//!   supports user-image blocks but the streaming-text reducer
//!   only accumulates `String`. Forwarding a binary payload here
//!   would silently lose it; a typed translation path is the fix.
//!
//! Each gap is exercised in the unit tests so a regression that
//! flips a skip into a panic is caught immediately.

use rebon_permissions::PermissionMode;
use rebon_render::hidden::is_transcript_hidden_tool;
use rebon_types::{
    format_system_time_iso_ms, ContentBlock, SessionId, SessionUpdate, SessionUpdateParams,
    ToolCallStatus, ToolKind,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

/// Tools that render in dedicated UI surfaces (task list, plan panel,
/// etc.) rather than inline in the transcript. Matches the list in
/// `rebon_render::project::is_hidden_tool_use`.
///
/// The `title` field from `SessionUpdate::ToolCall` is a formatted
/// string like `"TaskCreate: Fix the bug"` or `"TaskUpdate 3"`, so
/// we match on the prefix before the first space/colon.
/// Whether a streaming tool card is one of the tools that render somewhere
/// else — the task list, the plan panel, a permission dialog.
///
/// The list is [`rebon_render::hidden::TRANSCRIPT_HIDDEN_TOOLS`], the same one
/// the finished transcript reads. It was written out again here and had
/// drifted to eight names against thirteen, which meant a tool absent from the
/// transcript could still put a card on screen while it ran. What stays local
/// is the title split: a streaming title carries its subject
/// ("Agent: verify the fix") and the transcript's does not.
fn is_streaming_hidden_tool(title: &str) -> bool {
    let tool_name = title
        .split(|ch: char| ch == ':' || ch == ' ')
        .next()
        .unwrap_or(title);
    is_transcript_hidden_tool(tool_name)
}

fn looks_like_agent_tool(title: &str, raw_input: Option<&HashMap<String, Value>>) -> bool {
    if title == "Agent" || title.starts_with("Agent:") {
        return true;
    }
    raw_input.is_some_and(|input| {
        input.contains_key("subagent_type")
            || input.contains_key("subagentType")
            || (input.contains_key("prompt") && input.contains_key("description"))
    })
}

fn normalized_execute_tool_name(title: &str) -> &'static str {
    let first = title
        .split(|c: char| c.is_whitespace() || c == ':' || c == '(')
        .next()
        .unwrap_or_default();
    match first {
        "PowerShell" | "PowerShellTool" => "PowerShell",
        _ => "Bash",
    }
}

fn normalized_tool_name(
    kind: ToolKind,
    title: &str,
    raw_input: Option<&HashMap<String, Value>>,
) -> String {
    if title.starts_with('/') {
        return "Skill".to_string();
    }
    if matches!(kind, ToolKind::Other) && looks_like_agent_tool(title, raw_input) {
        return "Agent".to_string();
    }
    if title == "Workflow" || title.starts_with("Workflow ") || title.starts_with("Workflow:") {
        return "Workflow".to_string();
    }
    // The engine registers the tool under the `RunWorkflow` alias as well;
    // older recordings (and any path that skips `build_tool_call_title`) can
    // surface the alias as the title. Normalize it so every downstream
    // workflow special-case (progress merging, interrupt marking, the card
    // renderer) sees one canonical name.
    if title == "RunWorkflow"
        || title.starts_with("RunWorkflow ")
        || title.starts_with("RunWorkflow:")
    {
        return "Workflow".to_string();
    }
    match kind {
        ToolKind::Read => "Read".to_string(),
        ToolKind::Edit => "Edit".to_string(),
        ToolKind::Delete => "Delete".to_string(),
        ToolKind::Move => "Move".to_string(),
        ToolKind::Search => "Search".to_string(),
        ToolKind::Execute => normalized_execute_tool_name(title).to_string(),
        ToolKind::Think => "Think".to_string(),
        ToolKind::Fetch => "Fetch".to_string(),
        ToolKind::Other => {
            if title.is_empty() {
                "Tool".to_string()
            } else {
                title.to_string()
            }
        }
    }
}

fn normalized_title(title: String, raw_input: Option<&HashMap<String, Value>>) -> Option<String> {
    if raw_input.is_some() {
        Some(title)
    } else {
        None
    }
}

fn is_explore_agent_input(raw_input: Option<&HashMap<String, Value>>) -> bool {
    raw_input.is_some_and(|input| {
        input
            .get("subagent_type")
            .or_else(|| input.get("subagentType"))
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("Explore"))
    })
}

fn record_ultraplan_reexploration(
    app: &mut AppState,
    tool_name: &str,
    raw_input: Option<&HashMap<String, Value>>,
) {
    let Some(status) = app.ultraplan_status.as_mut() else {
        return;
    };
    if !matches!(
        status.phase,
        crate::session::ultraplan_run::UltraplanPhase::Executing
    ) {
        return;
    }
    if !status.context.as_ref().is_some_and(|ctx| ctx.plan_fidelity) {
        return;
    }
    let should_count = matches!(tool_name, "Glob" | "Grep")
        || (tool_name == "Agent" && is_explore_agent_input(raw_input));
    if should_count {
        status.execution_reexploration_count =
            status.execution_reexploration_count.saturating_add(1);
    }
}

fn extract_async_agent_launch(
    raw_output: &HashMap<String, Value>,
) -> Option<BackgroundAgentTaskRef> {
    BackgroundAgentTaskRef::from_raw_output_map(raw_output)
}

fn record_async_agent_launch(
    app: &mut AppState,
    tool_call_id: &str,
    raw_output: Option<&HashMap<String, Value>>,
) {
    let Some(task_ref) = raw_output.and_then(extract_async_agent_launch) else {
        return;
    };
    app.background_agent_tool_tasks
        .insert(tool_call_id.to_string(), task_ref);
}

fn merge_completed_write_path_into_file_index(
    app: &mut AppState,
    raw_output: Option<&HashMap<String, Value>>,
) {
    let Some(raw_output) = raw_output else {
        return;
    };
    if !raw_output
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|ty| matches!(ty, "create" | "update"))
    {
        return;
    }
    let Some(path) = raw_output.get("filePath").and_then(Value::as_str) else {
        return;
    };
    let cwd = Path::new(&app.cwd);
    let path = Path::new(path);
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let Ok(canonical_cwd) = cwd.canonicalize() else {
        return;
    };
    let Ok(canonical_path) = absolute_path.canonicalize() else {
        return;
    };
    if !canonical_path.is_file() {
        return;
    }
    let Ok(relative_path) = canonical_path.strip_prefix(&canonical_cwd) else {
        return;
    };
    let relative_path = relative_path.to_string_lossy().replace('\\', "/");
    app.file_index.merge(vec![relative_path]);
}

fn ensure_async_agent_tool_count(
    tool_name: &str,
    raw_output: Option<HashMap<String, Value>>,
) -> Option<HashMap<String, Value>> {
    let mut raw_output = raw_output?;
    if tool_name == "Agent"
        && raw_output
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status == "async_launched")
    {
        raw_output
            .entry("tool_call_count".to_string())
            .or_insert_with(|| json!(0));
        raw_output
            .entry("toolCallCount".to_string())
            .or_insert_with(|| json!(0));
    }
    Some(raw_output)
}

use rebon_tui::{
    reducer, Action, Message, SealedPrefixFlushPolicy, UserContentBlock, UserImageBlock,
    UserMessage, UserMessageInner, UserRole, UserTextBlock,
};

use crate::session::transcript_replay::BackgroundAgentTaskRef;
use crate::tui::app::AppState;

const DEFAULT_SPINNER_VERB: &str = "Thinking";

/// Force a sealed-prefix flush so completed overlay blocks move into the
/// transcript and become committable to scrollback. This is the *overflow
/// escape hatch*: its only caller invokes it when the live tail exceeds what
/// the inline viewport can show even after growing to the terminal-height cap.
///
/// It therefore uses `DrainClosedStableAndLiveText` unconditionally — dropping
/// the normal grouping hold-back and allowing the current live text chunk to
/// land in scrollback — so a long trailing collapsible tool cluster or answer
/// text block cannot stay stranded live and get clipped off the top of the
/// bottom-anchored viewport (which would never reach scrollback, since only
/// transcript rows are `insert_before`'d). The live text tail drains at
/// newline granularity (complete lines commit, the partial trailing line
/// stays live — see `flush_sealed_prefix`), so a repeated overflow drain
/// deposits clean line-aligned slabs instead of chopping the message at
/// whatever byte had streamed in by that frame. Completed tools still group via
/// `group_blocks_into_chunks` at commit time, so a turn that overflows the whole
/// terminal simply deposits its collapsed groups into scrollback in production
/// order instead of hoarding one ever-growing mega-cluster live. Normal
/// per-delta flushing still preserves live grouping through `apply_with_flush`'s
/// hold policy.
pub(crate) fn force_drain_overlay_sealed_prefix(app: &mut AppState) {
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    reducer(
        &mut app.rebon_tui,
        Action::FlushSealedPrefix {
            commit_timestamp: timestamp,
            policy: SealedPrefixFlushPolicy::DrainClosedStableAndLiveText,
        },
    );
}

/// Apply an overlay-mutating reducer action and immediately attempt
/// to drain the sealed prefix into the transcript. Keeps the streaming
/// overlay bounded to the still-in-flight tail so multi-step turns
/// no longer accumulate dozens of segments.
fn apply_with_flush(app: &mut AppState, action: Action) {
    reducer(&mut app.rebon_tui, action);
    let timestamp = format_system_time_iso_ms(SystemTime::now());
    let policy = if matches!(app.ui_mode, crate::ui_config::UiMode::Inline) {
        SealedPrefixFlushPolicy::DrainClosedStableHoldTrailingToolClusterUntilAssistantBoundary
    } else {
        SealedPrefixFlushPolicy::HoldBackTrailingToolCluster
    };
    reducer(
        &mut app.rebon_tui,
        Action::FlushSealedPrefix {
            commit_timestamp: timestamp,
            policy,
        },
    );
}

fn commit_queued_user_message(
    app: &mut AppState,
    uuid: String,
    content: Vec<ContentBlock>,
    image_paste_ids: Option<Vec<u32>>,
) {
    if app.rebon_tui.transcript.get(&uuid).is_some() {
        return;
    }

    let timestamp = format_system_time_iso_ms(SystemTime::now());
    reducer(&mut app.rebon_tui, Action::EndStreamingThinking);
    // Everything the model already said belongs ABOVE this message, and a
    // committed message always renders under the live overlay — so anything
    // left live here would render below an answer that came after it.
    //
    // The trailing text block is what gets stranded. `AskUserQuestion` renders
    // in its own dialog, so its `ToolCall` never reaches the overlay and
    // nothing closes the text the model wrote before asking; `DrainClosedStable`
    // only drains a terminal text block that some completed tool or closed
    // thinking block precedes, which that text has neither of. Sealing the live
    // tail is what puts the answer under the question, and it seals whole —
    // that block is finished, so there is no partial last line to hold back for
    // deltas that are not coming. Later deltas open a fresh block, which is
    // right: what the model says next it says after the answer.
    reducer(
        &mut app.rebon_tui,
        Action::FlushSealedPrefix {
            commit_timestamp: timestamp.clone(),
            policy: SealedPrefixFlushPolicy::DrainClosedStableAndSealLiveText,
        },
    );
    let user_content = content
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => {
                Some(UserContentBlock::Text(UserTextBlock { text: text.text }))
            }
            ContentBlock::Image(image) => {
                let mut source = serde_json::json!({
                    "type": "base64",
                    "media_type": image.mime_type,
                    "data": image.data,
                });
                if let Some(uri) = image.uri {
                    source["uri"] = serde_json::json!(uri);
                }
                Some(UserContentBlock::Image(UserImageBlock { source }))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if user_content.is_empty() {
        return;
    }

    reducer(
        &mut app.rebon_tui,
        Action::Commit(Message::User(UserMessage {
            uuid,
            timestamp,
            message: UserMessageInner {
                role: UserRole::User,
                content: user_content,
            },
            is_compact_summary: None,
            is_meta: None,
            is_visible_in_transcript_only: None,
            image_paste_ids,
            plan_content: None,
        })),
    );
    app.follow_transcript_tail = true;
}

fn is_visible_session_update(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::AgentMessageChunk { .. }
            | SessionUpdate::QueuedUserMessage { .. }
            | SessionUpdate::ToolCall { .. }
            | SessionUpdate::ToolCallUpdate { .. }
            | SessionUpdate::ThinkingDelta { .. }
            | SessionUpdate::ThinkingEnd
    )
}

/// Fold a single [`SessionUpdateParams`] into the local TUI's
/// rebon-tui transcript state.
pub fn translate_session_update(app: &mut AppState, params: SessionUpdateParams) {
    let session_id = params.session_id;
    if app.suppress_late_visible_updates_after_withdrawal
        && is_visible_session_update(&params.update)
    {
        tracing::debug!(
            %session_id,
            "rebon-cli: suppressing late visible update after withdrawn prompt"
        );
        app.rebon_tui.overlay.clear();
        return;
    }
    match params.update {
        SessionUpdate::AgentMessageChunk { content } => {
            translate_agent_message_chunk(app, &session_id, content);
        }
        SessionUpdate::QueuedUserMessage {
            uuid,
            content,
            image_paste_ids,
        } => {
            commit_queued_user_message(app, uuid, content, image_paste_ids);
        }
        SessionUpdate::ToolCall {
            tool_call_id,
            title,
            kind,
            status,
            content,
            locations,
            raw_input,
            raw_output,
        } => {
            // Entering plan mode is tracked before it is hidden, so the
            // footer can still update when the tool completes.
            if title == "EnterPlanMode" {
                app.pending_plan_mode_tool_ids
                    .insert(tool_call_id.clone(), PermissionMode::Plan);
            }
            // Tools that render in dedicated UI surfaces (task list, plan
            // panel, permission dialog) put no card in the overlay.
            if is_streaming_hidden_tool(&title) {
                app.hidden_tool_call_ids.insert(tool_call_id);
                return;
            }
            let tool_name = normalized_tool_name(kind, &title, raw_input.as_ref());
            record_ultraplan_reexploration(app, &tool_name, raw_input.as_ref());
            let raw_output = ensure_async_agent_tool_count(&tool_name, raw_output);
            record_async_agent_launch(app, &tool_call_id, raw_output.as_ref());
            tracing::info!(
                target: "stream_dbg",
                sess = %session_id,
                call_id = %tool_call_id,
                tool = %tool_name,
                ?status,
                "tui: ToolCall (StartToolUse)"
            );
            apply_with_flush(
                app,
                Action::StartToolUse {
                    call_id: tool_call_id,
                    tool_name,
                    kind,
                    initial_status: status,
                    initial_title: normalized_title(title, raw_input.as_ref()),
                    raw_input,
                    content,
                    locations,
                    raw_output,
                },
            );
        }
        SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status,
            title,
            content,
            locations,
            raw_output,
        } => {
            if matches!(status, Some(ToolCallStatus::Completed)) {
                merge_completed_write_path_into_file_index(app, raw_output.as_ref());
            }
            // Detect plan mode tool completion and flip the footer
            // indicator before the hidden-ID check drops the update.
            if matches!(status, Some(ToolCallStatus::Completed)) {
                if let Some(target_mode) = app.pending_plan_mode_tool_ids.remove(&tool_call_id) {
                    app.set_permission_mode(target_mode);
                    // Still hidden — fall through to the early return.
                }
            }
            if app.hidden_tool_call_ids.contains(&tool_call_id) {
                return;
            }
            tracing::info!(
                target: "stream_dbg",
                sess = %session_id,
                call_id = %tool_call_id,
                ?status,
                "tui: ToolCallUpdate"
            );
            let raw_output = ensure_async_agent_tool_count("Agent", raw_output);
            record_async_agent_launch(app, &tool_call_id, raw_output.as_ref());
            apply_with_flush(
                app,
                Action::UpdateToolUse {
                    call_id: tool_call_id,
                    status,
                    title,
                    content,
                    locations,
                    raw_output,
                },
            );
        }
        SessionUpdate::ToolCallAutoModeAllowed {
            tool_call_id,
            source,
        } => {
            // Display-only: remember the id and what allowed it so this call's
            // card (live now, committed later, replayed after /resume) can say
            // so. Nothing here reaches the model.
            tracing::debug!(
                %session_id,
                call_id = %tool_call_id,
                ?source,
                "rebon-cli: auto mode allowed a tool call without a dialog"
            );
            app.auto_mode_allowed_tool_ids.insert(tool_call_id, source);
        }
        SessionUpdate::Plan { entries } => {
            // store plan entries so the plan panel can render
            // them. Each push replaces the entire snapshot — the
            // engine always sends the full list.
            tracing::debug!(
                %session_id,
                entry_count = entries.len(),
                "rebon-cli: received Plan session update"
            );
            app.plan_entries = entries;
        }
        SessionUpdate::SlashCommands { commands } => {
            tracing::info!(
                %session_id,
                command_count = commands.len(),
                "rebon-cli: received slash commands for picker"
            );
            app.slash_commands = commands;
        }
        SessionUpdate::ConfigOptionUpdate { config_options } => {
            // store config options so the settings surface can
            // read them. Replaces the full snapshot each time.
            tracing::debug!(
                %session_id,
                option_count = config_options.len(),
                "rebon-cli: received ConfigOptionUpdate session update"
            );
            app.config_options = config_options;
        }
        SessionUpdate::SessionInfoUpdate { title, meta, .. } => {
            if let Some(notice) = meta
                .as_ref()
                .and_then(|meta| meta.get("uiNotice"))
                .and_then(serde_json::Value::as_str)
            {
                super::runner::inject_system_message(app, "info", notice);
            }
            // store the session title for the status bar /
            // terminal window title display.
            tracing::debug!(
                %session_id,
                ?title,
                "rebon-cli: received SessionInfoUpdate session update"
            );
            if let Some(t) = title {
                app.session_title = Some(t);
            }
        }
        SessionUpdate::ThinkingDelta { text } => {
            apply_with_flush(app, Action::AppendStreamingThinking(text));
        }
        SessionUpdate::ThinkingEnd => {
            apply_with_flush(app, Action::EndStreamingThinking);
        }
        SessionUpdate::ContextReset { plan } => {
            crate::tui::runner::apply_context_reset_to_tui(app, plan);
        }
        SessionUpdate::CompactingStarted { .. } => {
            // Override the spinner verb while compaction is in
            // progress. Capture the in-flight random verb first so
            // CompactingDone can restore the original wording
            // immediately without waiting for another loading
            // transition.
            if app.spinner_verb_before_compacting.is_none() {
                app.spinner_verb_before_compacting = Some(app.spinner_verb.clone());
            }
            app.spinner_verb = "Compacting".to_string();
        }
        SessionUpdate::CompactingDone {
            messages_after,
            used_model,
        } => {
            app.spinner_verb = app
                .spinner_verb_before_compacting
                .take()
                .unwrap_or_else(|| DEFAULT_SPINNER_VERB.to_string());
            let method = if used_model {
                "model-based summarisation"
            } else {
                "truncation"
            };
            let content = format!(
                "Context was automatically compacted via {method}. \
                 History reduced to {messages_after} messages."
            );
            crate::tui::runner::inject_system_message(app, "compact", &content);
            app.follow_transcript_tail = true;
        }
        SessionUpdate::TokenUsage {
            input_tokens,
            output_tokens,
        } => {
            app.streaming_token_count = output_tokens;
            app.usage_mut()
                .note_streaming_usage(input_tokens, output_tokens);
        }
    }
}

/// Rendering effects produced by the remote read-only projector. The runner
/// records them against the current queued-user boundary so transcript refresh
/// can retire the overlay without consulting or mutating local control state.
#[derive(Debug, Default)]
pub(crate) struct RemoteProjectionEffect {
    pub(crate) text_delta: Option<String>,
    pub(crate) thinking_delta: Option<String>,
    pub(crate) visible_tool_call_id: Option<String>,
    pub(crate) overlay_cleared: bool,
}

/// Restore display snapshots while initializing a remote attachment without
/// replaying historical streaming content. ContextReset is represented here as
/// a plan reset only; its transcript/overlay reset is applied only when it lies
/// inside the current turn suffix.
pub(crate) fn project_remote_session_snapshot_update(app: &mut AppState, update: &SessionUpdate) {
    match update {
        SessionUpdate::Plan { entries } => app.plan_entries = entries.clone(),
        SessionUpdate::TokenUsage {
            input_tokens,
            output_tokens,
        } => {
            app.streaming_token_count = *output_tokens;
            app.usage_mut()
                .note_streaming_usage(*input_tokens, *output_tokens);
        }
        SessionUpdate::SessionInfoUpdate {
            title: Some(title), ..
        } => app.session_title = Some(title.clone()),
        SessionUpdate::ContextReset { .. } => app.plan_entries.clear(),
        SessionUpdate::CompactingStarted { .. } => {
            if app.spinner_verb_before_compacting.is_none() {
                app.spinner_verb_before_compacting = Some(app.spinner_verb.clone());
            }
            app.spinner_verb = "Compacting".to_string();
        }
        SessionUpdate::CompactingDone { .. } => {
            app.spinner_verb = app
                .spinner_verb_before_compacting
                .take()
                .unwrap_or_else(|| DEFAULT_SPINNER_VERB.to_string());
        }
        _ => {}
    }
}

/// Project a remote worker update into display-only TUI state.
///
/// Unlike [`translate_session_update`], this path never writes local permission
/// mode, local hidden/pending tool ids, ultraplan counters, file index, async
/// agent registries, slash commands, or config controls. Hidden ids are owned by
/// the [`RemoteBackgroundAttachment`](crate::background::RemoteBackgroundAttachment)
/// passed by the runner.
pub(crate) fn project_remote_session_update(
    app: &mut AppState,
    params: SessionUpdateParams,
    remote_hidden_tool_call_ids: &mut HashSet<String>,
) -> RemoteProjectionEffect {
    let session_id = params.session_id;
    let mut effect = RemoteProjectionEffect::default();
    match params.update {
        SessionUpdate::AgentMessageChunk { content } => {
            if let ContentBlock::Text(text) = &content {
                if !text.text.is_empty() {
                    effect.text_delta = Some(text.text.clone());
                }
            }
            translate_agent_message_chunk(app, &session_id, content);
        }
        // The runner consumes this as a turn boundary. Persisted transcript
        // replay owns the user row, so remote projection must not commit it.
        SessionUpdate::QueuedUserMessage { .. } => {}
        SessionUpdate::ToolCall {
            tool_call_id,
            title,
            kind,
            status,
            content,
            locations,
            raw_input,
            raw_output,
        } => {
            if is_streaming_hidden_tool(&title) {
                remote_hidden_tool_call_ids.insert(tool_call_id);
                return effect;
            }
            let tool_name = normalized_tool_name(kind, &title, raw_input.as_ref());
            let raw_output = ensure_async_agent_tool_count(&tool_name, raw_output);
            let projected_id = tool_call_id.clone();
            apply_with_flush(
                app,
                Action::StartToolUse {
                    call_id: tool_call_id,
                    tool_name,
                    kind,
                    initial_status: status,
                    initial_title: normalized_title(title, raw_input.as_ref()),
                    raw_input,
                    content,
                    locations,
                    raw_output,
                },
            );
            effect.visible_tool_call_id = Some(projected_id);
        }
        SessionUpdate::ToolCallUpdate {
            tool_call_id,
            status,
            title,
            content,
            locations,
            raw_output,
        } => {
            if remote_hidden_tool_call_ids.contains(&tool_call_id) {
                return effect;
            }
            let projected_id = tool_call_id.clone();
            let raw_output = ensure_async_agent_tool_count("Agent", raw_output);
            apply_with_flush(
                app,
                Action::UpdateToolUse {
                    call_id: tool_call_id,
                    status,
                    title,
                    content,
                    locations,
                    raw_output,
                },
            );
            effect.visible_tool_call_id = Some(projected_id);
        }
        SessionUpdate::ToolCallAutoModeAllowed {
            tool_call_id,
            source,
        } => {
            app.auto_mode_allowed_tool_ids.insert(tool_call_id, source);
        }
        SessionUpdate::Plan { entries } => app.plan_entries = entries,
        // Slash/config updates are controls for the local engine session, not
        // display state owned by the attached worker.
        SessionUpdate::SlashCommands { .. } | SessionUpdate::ConfigOptionUpdate { .. } => {}
        SessionUpdate::SessionInfoUpdate { title, .. } => {
            if let Some(title) = title {
                app.session_title = Some(title);
            }
        }
        SessionUpdate::ThinkingDelta { text } => {
            if !text.is_empty() {
                effect.thinking_delta = Some(text.clone());
            }
            apply_with_flush(app, Action::AppendStreamingThinking(text));
        }
        SessionUpdate::ThinkingEnd => {
            apply_with_flush(app, Action::EndStreamingThinking);
        }
        SessionUpdate::ContextReset { plan } => {
            app.rebon_tui.transcript.clear();
            app.rebon_tui.overlay.clear();
            app.plan_entries.clear();
            app.follow_transcript_tail = true;
            if let Some(plan) = plan {
                crate::tui::runner::inject_plan_card(app, &plan);
            }
            effect.overlay_cleared = true;
        }
        SessionUpdate::CompactingStarted { .. } => {
            if app.spinner_verb_before_compacting.is_none() {
                app.spinner_verb_before_compacting = Some(app.spinner_verb.clone());
            }
            app.spinner_verb = "Compacting".to_string();
        }
        SessionUpdate::CompactingDone {
            messages_after,
            used_model,
        } => {
            app.spinner_verb = app
                .spinner_verb_before_compacting
                .take()
                .unwrap_or_else(|| DEFAULT_SPINNER_VERB.to_string());
            let method = if used_model {
                "model-based summarisation"
            } else {
                "truncation"
            };
            crate::tui::runner::inject_system_message(
                app,
                "compact",
                &format!(
                    "Context was automatically compacted via {method}. \
                     History reduced to {messages_after} messages."
                ),
            );
            app.follow_transcript_tail = true;
        }
        SessionUpdate::TokenUsage {
            input_tokens,
            output_tokens,
        } => {
            app.streaming_token_count = output_tokens;
            app.usage_mut()
                .note_streaming_usage(input_tokens, output_tokens);
        }
    }
    effect
}

/// Translate an [`SessionUpdate::AgentMessageChunk`] payload.
///
/// Only `ContentBlock::Text` is forwarded — binary content
/// (image / audio / resource) would otherwise be silently dropped
/// by the reducer's string accumulator.
fn translate_agent_message_chunk(
    app: &mut AppState,
    session_id: &SessionId,
    content: ContentBlock,
) {
    match content {
        ContentBlock::Text(text) => {
            // Skip genuinely-empty string chunks (a zero-length
            // delta is a pure no-op). We deliberately do NOT call
            // `is_empty_assistant_message_text` here: that helper
            // trims whitespace, so it would classify a lone "\n"
            // delta as empty and drop it — which is exactly the
            // bug that made markdown list items between newlines
            // render on the same line ("- item- item" instead of
            // two separate bullets). The check belongs
            // at the projection stage (`project_assistant_text_message`),
            // where it decides whether a FULL accumulated message
            // is blank enough to hide from the transcript; it must
            // not run on individual streaming deltas, because a
            // whitespace delta is legitimate content being built
            // up between meaningful chunks.
            if text.text.is_empty() {
                tracing::trace!(
                    %session_id,
                    "rebon-cli: skipping zero-length assistant text chunk"
                );
                return;
            }
            let len = text.text.len();
            apply_with_flush(app, Action::AppendStreamingText(text.text));
            tracing::info!(
                target: "stream_dbg",
                sess = %session_id,
                delta_len = len,
                overlay_blocks = app.rebon_tui.overlay.blocks.len(),
                overlay_total_text = app
                    .rebon_tui
                    .overlay
                    .combined_streaming_text()
                    .map(|s| s.len())
                    .unwrap_or(0),
                "tui: append streaming text"
            );
        }
        ContentBlock::Image(_) => {
            tracing::debug!(
                %session_id,
                "rebon-cli: dropping AgentMessageChunk(image) (no translation path yet)"
            );
        }
        ContentBlock::Audio(_) => {
            tracing::debug!(
                %session_id,
                "rebon-cli: dropping AgentMessageChunk(audio) (no translation path yet)"
            );
        }
        ContentBlock::Resource(_) => {
            tracing::debug!(
                %session_id,
                "rebon-cli: dropping AgentMessageChunk(resource) (no translation path yet)"
            );
        }
        ContentBlock::ResourceLink(_) => {
            tracing::debug!(
                %session_id,
                "rebon-cli: dropping AgentMessageChunk(resource_link) (no translation path yet)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::{
        ConfigOption, ConfigOptionType, ConfigOptionValue, ContentBlock, ImageContent, PlanEntry,
        PlanEntryPriority, PlanEntryStatus, SessionId, SessionUpdate, SessionUpdateParams,
        TextContent, ToolCallLocation, ToolCallStatus, ToolKind,
    };
    use serde_json::json;
    use std::collections::HashMap;

    /// The overlay hides what the transcript hides. Two lists that were
    /// supposed to say the same thing had drifted to eight names against
    /// thirteen, so a tool could be absent from the finished transcript and
    /// present as a card while it ran.
    #[test]
    fn the_streaming_overlay_hides_every_tool_the_transcript_hides() {
        for name in rebon_render::hidden::TRANSCRIPT_HIDDEN_TOOLS {
            assert!(
                is_streaming_hidden_tool(name),
                "{name} renders no transcript card but shows one while it runs"
            );
        }
        // Still a prefix match on the title, which is how a titled call
        // ("Agent: verify the fix") is recognised.
        assert!(is_streaming_hidden_tool("TaskCreate: ship it"));
        assert!(!is_streaming_hidden_tool("Read"));
    }

    fn raw_input(path: &str) -> HashMap<String, Value> {
        HashMap::from([("path".into(), json!(path))])
    }

    fn raw_output(size: i64) -> HashMap<String, Value> {
        HashMap::from([("bytes".into(), json!(size))])
    }

    fn write_raw_output(path: &Path) -> HashMap<String, Value> {
        HashMap::from([
            ("type".into(), json!("create")),
            (
                "filePath".into(),
                json!(path.to_string_lossy().replace('\\', "/")),
            ),
        ])
    }

    fn sess() -> SessionId {
        "sess-s3-test".into()
    }

    fn params(update: SessionUpdate) -> SessionUpdateParams {
        SessionUpdateParams {
            session_id: sess(),
            update,
        }
    }

    #[test]
    fn execute_tool_name_preserves_powershell_titles() {
        assert_eq!(
            normalized_tool_name(ToolKind::Execute, "PowerShell Get-ChildItem", None),
            "PowerShell"
        );
        assert_eq!(
            normalized_tool_name(ToolKind::Execute, "Bash ls", None),
            "Bash"
        );
    }

    fn executing_plan_fidelity_app() -> AppState {
        let mut app = AppState::new();
        app.ultraplan_status = Some(crate::session::ultraplan_run::UltraplanStatus {
            run_id: "run-1".into(),
            phase: crate::session::ultraplan_run::UltraplanPhase::Executing,
            task_title: "task".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: Some(
                rebon_types::UltraplanContext::ultrawork_execution_controller_turn(
                    "run-1",
                    rebon_types::PolicyMode::Enforce,
                ),
            ),
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        app
    }

    #[test]
    fn remote_tool_projection_isolates_local_control_and_registries() {
        let temp = tempfile::tempdir().unwrap();
        let written = temp.path().join("remote_write.rs");
        std::fs::write(&written, "remote\n").unwrap();
        let mut app = executing_plan_fidelity_app();
        app.cwd = temp.path().to_string_lossy().to_string();
        app.set_permission_mode(PermissionMode::Default);
        app.pending_plan_mode_tool_ids
            .insert("local-plan".into(), PermissionMode::Plan);
        app.hidden_tool_call_ids.insert("local-hidden".into());
        let mut remote_hidden = HashSet::new();

        project_remote_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "remote-enter-plan".into(),
                title: "EnterPlanMode".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: Some(HashMap::new()),
                raw_output: None,
            }),
            &mut remote_hidden,
        );
        project_remote_session_update(
            &mut app,
            params(SessionUpdate::ToolCallUpdate {
                tool_call_id: "remote-enter-plan".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: None,
            }),
            &mut remote_hidden,
        );
        let write_effect = project_remote_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "remote-write".into(),
                title: "Write remote_write.rs".into(),
                kind: ToolKind::Edit,
                status: ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: Some(raw_input("remote_write.rs")),
                raw_output: None,
            }),
            &mut remote_hidden,
        );
        assert_eq!(
            write_effect.visible_tool_call_id.as_deref(),
            Some("remote-write")
        );
        assert!(app
            .rebon_tui
            .overlay
            .find_tool_use("remote-write")
            .is_some());
        project_remote_session_update(
            &mut app,
            params(SessionUpdate::ToolCallUpdate {
                tool_call_id: "remote-write".into(),
                status: Some(ToolCallStatus::Completed),
                title: None,
                content: None,
                locations: None,
                raw_output: Some(write_raw_output(&written)),
            }),
            &mut remote_hidden,
        );
        project_remote_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "remote-agent".into(),
                title: "Agent".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([
                    ("subagent_type".into(), json!("Explore")),
                    ("description".into(), json!("inspect")),
                    ("prompt".into(), json!("inspect")),
                ])),
                raw_output: Some(HashMap::from([
                    ("status".into(), json!("async_launched")),
                    ("task_id".into(), json!("remote-task")),
                    ("agent_id".into(), json!("remote-agent-id")),
                ])),
            }),
            &mut remote_hidden,
        );

        assert_eq!(app.permission_mode, PermissionMode::Default);
        assert_eq!(
            app.pending_plan_mode_tool_ids.get("local-plan"),
            Some(&PermissionMode::Plan)
        );
        assert_eq!(app.pending_plan_mode_tool_ids.len(), 1);
        assert_eq!(app.hidden_tool_call_ids.len(), 1);
        assert!(app.hidden_tool_call_ids.contains("local-hidden"));
        assert!(remote_hidden.contains("remote-enter-plan"));
        assert_eq!(
            app.ultraplan_status
                .as_ref()
                .unwrap()
                .execution_reexploration_count,
            0
        );
        assert!(app.background_agent_tool_tasks.is_empty());
        assert!(app.file_index.search("remote_write", 5).is_empty());
        assert!(app
            .rebon_tui
            .overlay
            .find_tool_use("remote-agent")
            .is_some());
    }

    #[test]
    fn remote_display_projection_includes_plan_usage_title_and_visual_context_reset() {
        let mut app = executing_plan_fidelity_app();
        let original_phase = app.ultraplan_status.as_ref().unwrap().phase;
        let mut remote_hidden = HashSet::new();
        project_remote_session_update(
            &mut app,
            params(SessionUpdate::Plan {
                entries: vec![PlanEntry {
                    content: "remote step".into(),
                    priority: PlanEntryPriority::High,
                    status: PlanEntryStatus::InProgress,
                }],
            }),
            &mut remote_hidden,
        );
        project_remote_session_update(
            &mut app,
            params(SessionUpdate::TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
            }),
            &mut remote_hidden,
        );
        project_remote_session_update(
            &mut app,
            params(SessionUpdate::SessionInfoUpdate {
                title: Some("Remote session".into()),
                updated_at: None,
                meta: None,
            }),
            &mut remote_hidden,
        );

        assert_eq!(app.plan_entries[0].content, "remote step");
        assert_eq!(app.streaming_token_count, 34);
        assert_eq!(app.usage().last_turn.input_tokens, 12);
        assert_eq!(app.session_title.as_deref(), Some("Remote session"));

        let effect = project_remote_session_update(
            &mut app,
            params(SessionUpdate::ContextReset {
                plan: Some("execute remote plan".into()),
            }),
            &mut remote_hidden,
        );
        assert!(effect.overlay_cleared);
        assert!(app.plan_entries.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 1);
        assert_eq!(app.ultraplan_status.as_ref().unwrap().phase, original_phase);
    }

    /// End-to-end regression for the stuck `run wf_…` workflow card: the
    /// engine titles the aliased tool call `RunWorkflow …`, which must
    /// normalize to the canonical `Workflow` overlay name so streamed
    /// `workflow_progress` payloads MERGE into `workflowProgress.entries`
    /// instead of replacing raw_output (which left the live card showing
    /// only the launch ack for the whole run).
    #[test]
    fn run_workflow_alias_tool_call_accumulates_progress_entries() {
        let mut app = AppState::default();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "wf-1".into(),
                title: "RunWorkflow core-gameplay-research".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([("script".into(), json!("workflow()"))])),
                raw_output: None,
            }),
        );

        let progress = |sequence: u64, entry: serde_json::Value| {
            HashMap::from([
                ("type".into(), json!("workflow_progress")),
                ("runId".into(), json!("wf_alias")),
                ("workflowName".into(), json!("core-gameplay-research")),
                ("sequence".into(), json!(sequence)),
                ("entry".into(), entry),
            ])
        };
        for (sequence, entry) in [
            (
                1,
                json!({ "type": "phase", "title": "Research", "state": "start" }),
            ),
            (
                2,
                json!({ "type": "agent", "index": 1, "state": "start", "phaseTitle": "Research", "label": "research:battle" }),
            ),
        ] {
            translate_session_update(
                &mut app,
                params(SessionUpdate::ToolCallUpdate {
                    tool_call_id: "wf-1".into(),
                    status: None,
                    title: None,
                    content: None,
                    locations: None,
                    raw_output: Some(progress(sequence, entry)),
                }),
            );
        }

        let tool = app
            .rebon_tui
            .overlay
            .find_tool_use("wf-1")
            .expect("workflow tool in overlay");
        assert_eq!(tool.tool_name, "Workflow");
        let entries = tool
            .raw_output
            .as_ref()
            .expect("raw_output")
            .get("workflowProgress")
            .and_then(serde_json::Value::as_object)
            .and_then(|progress| progress.get("entries"))
            .and_then(serde_json::Value::as_array)
            .expect("accumulated workflowProgress entries");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn executing_plan_fidelity_reexploration_tools_increment_count() {
        let mut app = executing_plan_fidelity_app();

        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "glob".into(),
                title: "Glob".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(HashMap::new()),
                raw_output: None,
            }),
        );
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "grep".into(),
                title: "Grep".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(HashMap::new()),
                raw_output: None,
            }),
        );
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "agent".into(),
                title: "Agent".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([("subagent_type".into(), json!("Explore"))])),
                raw_output: None,
            }),
        );

        assert_eq!(
            app.ultraplan_status
                .as_ref()
                .unwrap()
                .execution_reexploration_count,
            3
        );
    }

    #[test]
    fn non_reexploration_tools_do_not_increment_count() {
        let mut planning = executing_plan_fidelity_app();
        planning.ultraplan_status.as_mut().unwrap().phase =
            crate::session::ultraplan_run::UltraplanPhase::Researching;
        translate_session_update(
            &mut planning,
            params(SessionUpdate::ToolCall {
                tool_call_id: "planning-glob".into(),
                title: "Glob".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(HashMap::new()),
                raw_output: None,
            }),
        );

        let mut executing = executing_plan_fidelity_app();
        for (id, title, kind, raw_input) in [
            ("read", "Read", ToolKind::Read, HashMap::new()),
            (
                "agent",
                "Agent",
                ToolKind::Other,
                HashMap::from([("subagent_type".into(), json!("implementation"))]),
            ),
        ] {
            translate_session_update(
                &mut executing,
                params(SessionUpdate::ToolCall {
                    tool_call_id: id.into(),
                    title: title.into(),
                    kind,
                    status: ToolCallStatus::Pending,
                    content: None,
                    locations: None,
                    raw_input: Some(raw_input),
                    raw_output: None,
                }),
            );
        }

        assert_eq!(
            planning
                .ultraplan_status
                .as_ref()
                .unwrap()
                .execution_reexploration_count,
            0
        );
        assert_eq!(
            executing
                .ultraplan_status
                .as_ref()
                .unwrap()
                .execution_reexploration_count,
            0
        );
    }

    #[test]
    fn agent_message_chunk_text_appends_streaming_text() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "hello ".into(),
                    annotations: None,
                }),
            }),
        );
        translate_session_update(
            &mut app,
            params(SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "world".into(),
                    annotations: None,
                }),
            }),
        );

        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("hello world")
        );
        assert!(app.rebon_tui.transcript.is_empty());
    }

    #[test]
    fn agent_message_chunk_preserves_lone_newline_delta_between_list_items() {
        // Regression for the "列表之间的换行会被吞掉" bug:
        // the old code filtered streaming deltas through
        // is_empty_assistant_message_text, which trims whitespace,
        // so a lone "\n" chunk between two list-item chunks was
        // dropped. Result: `- A\n- B` rendered as `- A- B` on one
        // line. The fix limits the per-chunk skip to genuinely
        // empty strings.
        let mut app = AppState::new();
        for chunk in ["- 并行任务风格的拆分", "\n", "- 故意改错一版"] {
            translate_session_update(
                &mut app,
                params(SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text(TextContent {
                        text: chunk.into(),
                        annotations: None,
                    }),
                }),
            );
        }
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("- 并行任务风格的拆分\n- 故意改错一版")
        );
    }

    #[test]
    fn agent_message_chunk_preserves_multiple_consecutive_newline_deltas() {
        // A sequence of whitespace-only deltas (blank line
        // between paragraphs) must also survive in-flight.
        let mut app = AppState::new();
        for chunk in ["paragraph one", "\n", "\n", "paragraph two"] {
            translate_session_update(
                &mut app,
                params(SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text(TextContent {
                        text: chunk.into(),
                        annotations: None,
                    }),
                }),
            );
        }
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("paragraph one\n\nparagraph two")
        );
    }

    #[test]
    fn agent_message_chunk_preserves_whitespace_only_delta_with_spaces_and_tabs() {
        // Spaces and tabs inside deltas (e.g. indentation for
        // nested list items) must also survive.
        let mut app = AppState::new();
        for chunk in ["- outer", "\n", "  - inner", "\n", "- outer2"] {
            translate_session_update(
                &mut app,
                params(SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text(TextContent {
                        text: chunk.into(),
                        annotations: None,
                    }),
                }),
            );
        }
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("- outer\n  - inner\n- outer2")
        );
    }

    #[test]
    fn agent_message_chunk_skips_zero_length_delta_without_mutating_state() {
        // Empty string chunks remain a no-op — there's no
        // content to append, and calling the reducer would
        // uselessly create a Text block if one didn't exist.
        let mut app = AppState::new();
        reducer(&mut app.rebon_tui, Action::SetStreamingText("seed".into()));
        translate_session_update(
            &mut app,
            params(SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: String::new(),
                    annotations: None,
                }),
            }),
        );
        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("seed")
        );
    }

    #[test]
    fn non_text_agent_message_chunks_are_dropped_without_mutating_state() {
        let mut app = AppState::new();
        // Pre-seed some streaming text so we can prove the
        // drop-on-the-floor path did not accidentally clear it.
        reducer(
            &mut app.rebon_tui,
            Action::SetStreamingText("keep me".into()),
        );

        translate_session_update(
            &mut app,
            params(SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Image(ImageContent {
                    mime_type: "image/png".into(),
                    data: String::new(),
                    uri: None,
                    annotations: None,
                }),
            }),
        );

        assert_eq!(
            app.rebon_tui.overlay.combined_streaming_text().as_deref(),
            Some("keep me")
        );
    }

    /// Streamed assistant text lands above the answer that followed it.
    ///
    /// `AskUserQuestion` renders in its own dialog, so its `ToolCall` never
    /// reaches the overlay and nothing closes the text block the model wrote
    /// before asking. That text was still live when the answer committed, and a
    /// committed message always renders under the transcript while live blocks
    /// render after it — so the answer jumped above the message it answered.
    #[test]
    fn ask_user_answer_commits_under_the_text_that_asked() {
        let mut app = AppState::new();

        translate_session_update(
            &mut app,
            params(SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "Which database should we use?".into(),
                    annotations: None,
                }),
            }),
        );
        translate_session_update(
            &mut app,
            params(SessionUpdate::QueuedUserMessage {
                uuid: "ask-answer-order".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "Answered questions:
- Which database should we use?
  Answer: Postgres"
                        .into(),
                    annotations: None,
                })],
                image_paste_ids: None,
            }),
        );

        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(
            matches!(&rows[0], Message::Assistant(_)),
            "the asking text commits first: {rows:?}"
        );
        match &rows[1] {
            Message::User(user) => assert_eq!(user.uuid, "ask-answer-order"),
            other => panic!("answer should follow the question: {other:?}"),
        }
        assert!(
            app.rebon_tui
                .overlay
                .combined_streaming_text()
                .is_none_or(|text| text.trim().is_empty()),
            "the drained text must not also stay live"
        );
    }

    #[test]
    fn ask_user_answer_update_commits_transcript_only_text() {
        let mut app = AppState::new();

        translate_session_update(
            &mut app,
            params(SessionUpdate::QueuedUserMessage {
                uuid: "ask-answer-1".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text:
                        "Answered questions:\n- Which database should we use?\n  Answer: Postgres"
                            .into(),
                    annotations: None,
                })],
                image_paste_ids: None,
            }),
        );

        assert_eq!(app.rebon_tui.transcript.len(), 1);
        match &app.rebon_tui.transcript.rows()[0] {
            Message::User(user) => {
                assert_eq!(user.uuid, "ask-answer-1");
                match &user.message.content[0] {
                    UserContentBlock::Text(text) => {
                        assert!(text.text.contains("Which database should we use?"));
                        assert!(text.text.contains("Answer: Postgres"));
                    }
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            other => panic!("expected user row, got {other:?}"),
        }
    }

    #[test]
    fn queued_user_message_commits_text_and_image_to_transcript() {
        let mut app = AppState::new();

        translate_session_update(
            &mut app,
            params(SessionUpdate::QueuedUserMessage {
                uuid: "queued-1".into(),
                content: vec![
                    ContentBlock::Text(TextContent {
                        text: "look at this".into(),
                        annotations: None,
                    }),
                    ContentBlock::Image(ImageContent {
                        mime_type: "image/png".into(),
                        data: "AAAA".into(),
                        uri: None,
                        annotations: None,
                    }),
                ],
                image_paste_ids: Some(vec![7]),
            }),
        );

        assert_eq!(app.rebon_tui.transcript.len(), 1);
        match &app.rebon_tui.transcript.rows()[0] {
            Message::User(user) => {
                assert_eq!(user.uuid, "queued-1");
                assert_eq!(user.image_paste_ids, Some(vec![7]));
                assert_eq!(user.message.content.len(), 2);
                match &user.message.content[0] {
                    UserContentBlock::Text(text) => assert_eq!(text.text, "look at this"),
                    other => panic!("expected text block, got {other:?}"),
                }
                match &user.message.content[1] {
                    UserContentBlock::Image(image) => {
                        assert_eq!(image.source["media_type"], json!("image/png"));
                        assert_eq!(image.source["data"], json!("AAAA"));
                    }
                    other => panic!("expected image block, got {other:?}"),
                }
            }
            other => panic!("expected queued user row, got {other:?}"),
        }
    }

    #[test]
    fn automatic_model_routing_notice_is_rendered_as_ui_only_system_row() {
        let mut app = AppState::new();
        let notice = rebon_core::model_routing::selection_notice("selected", None);
        translate_session_update(
            &mut app,
            params(rebon_core::model_routing::notice_update(notice.clone())),
        );
        assert!(!notice.contains("effort"));
        let rows = app.rebon_tui.transcript.rows();
        assert_eq!(rows.len(), 1);
        let Message::System(message) = &rows[0] else {
            panic!("expected UI-only system row")
        };
        assert_eq!(message.content.as_deref(), Some(notice.as_str()));
    }

    #[test]
    fn queued_user_message_keeps_completed_non_groupable_tool_committed_before_user_row() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "tool-1".into(),
                title: "Bash ls".into(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            }),
        );
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCallUpdate {
                tool_call_id: "tool-1".into(),
                status: Some(ToolCallStatus::Completed),
                title: Some("Bash ls - completed".into()),
                content: None,
                locations: None,
                raw_output: None,
            }),
        );
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 0);
        assert_eq!(app.rebon_tui.transcript.len(), 1);

        translate_session_update(
            &mut app,
            params(SessionUpdate::QueuedUserMessage {
                uuid: "queued-2".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "next please".into(),
                    annotations: None,
                })],
                image_paste_ids: None,
            }),
        );

        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 0);
        assert_eq!(app.rebon_tui.transcript.len(), 2);
        match &app.rebon_tui.transcript.rows()[0] {
            Message::Assistant(assistant) => assert!(matches!(
                &assistant.message.content[0],
                rebon_tui::AssistantContentBlock::ToolUse(tool) if tool.id == "tool-1"
            )),
            other => panic!("expected completed tool row first, got {other:?}"),
        }
        match app.rebon_tui.transcript.rows().last().unwrap() {
            Message::User(user) => assert_eq!(user.uuid, "queued-2"),
            other => panic!("expected queued user row last, got {other:?}"),
        }
    }

    #[test]
    fn queued_user_message_flushes_completed_tool_after_active_thinking() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ThinkingDelta {
                text: "reasoning".into(),
            }),
        );
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "tool-1".into(),
                title: "Read Cargo.toml".into(),
                kind: ToolKind::Read,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(raw_input("Cargo.toml")),
                raw_output: None,
            }),
        );
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCallUpdate {
                tool_call_id: "tool-1".into(),
                status: Some(ToolCallStatus::Completed),
                title: Some("Read Cargo.toml".into()),
                content: None,
                locations: None,
                raw_output: Some(raw_output(42)),
            }),
        );
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);

        translate_session_update(
            &mut app,
            params(SessionUpdate::QueuedUserMessage {
                uuid: "queued-3".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "next please".into(),
                    annotations: None,
                })],
                image_paste_ids: None,
            }),
        );

        assert!(app.rebon_tui.overlay.is_empty());
        assert_eq!(app.rebon_tui.transcript.len(), 2);
        match &app.rebon_tui.transcript.rows()[0] {
            Message::Assistant(assistant) => {
                assert!(matches!(
                    &assistant.message.content[0],
                    rebon_tui::AssistantContentBlock::Thinking(thinking)
                        if thinking.thinking == "reasoning"
                ));
                assert!(matches!(
                    &assistant.message.content[1],
                    rebon_tui::AssistantContentBlock::ToolUse(tool) if tool.id == "tool-1"
                ));
            }
            other => panic!("expected flushed assistant row first, got {other:?}"),
        }
        match &app.rebon_tui.transcript.rows()[1] {
            Message::User(user) => assert_eq!(user.uuid, "queued-3"),
            other => panic!("expected queued user row last, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_starts_streaming_tool_use_overlay() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "tool-1".into(),
                title: "Read Cargo.toml".into(),
                kind: ToolKind::Read,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(raw_input("Cargo.toml")),
                raw_output: None,
            }),
        );

        assert!(app.rebon_tui.transcript.is_empty());
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);
        let tool = app.rebon_tui.overlay.find_tool_use("tool-1").unwrap();
        assert_eq!(tool.call_id, "tool-1");
        assert_eq!(tool.tool_name, "Read");
        assert_eq!(tool.status, ToolCallStatus::Pending);
        assert_eq!(tool.title.as_deref(), Some("Read Cargo.toml"));
        assert_eq!(
            tool.raw_input.as_ref().unwrap()["path"],
            json!("Cargo.toml")
        );
    }

    #[test]
    fn skill_tool_call_keeps_skill_tool_name_and_slash_title() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "skill-1".into(),
                title: "/imagegen".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([("skill".into(), json!("imagegen"))])),
                raw_output: None,
            }),
        );

        let tool = app.rebon_tui.overlay.find_tool_use("skill-1").unwrap();
        assert_eq!(tool.tool_name, "Skill");
        assert_eq!(tool.title.as_deref(), Some("/imagegen"));
    }

    #[test]
    fn explorer_agent_title_normalizes_to_agent_tool_name() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "agent-1".into(),
                title: "Explore: compare permission modes".into(),
                kind: ToolKind::Other,
                status: ToolCallStatus::InProgress,
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([
                    ("subagent_type".into(), json!("Explore")),
                    ("description".into(), json!("compare permission modes")),
                    ("prompt".into(), json!("compare permission modes")),
                ])),
                raw_output: None,
            }),
        );

        let tool = app.rebon_tui.overlay.find_tool_use("agent-1").unwrap();
        assert_eq!(tool.tool_name, "Agent");
        assert_eq!(
            tool.title.as_deref(),
            Some("Explore: compare permission modes")
        );
    }

    #[test]
    fn completed_write_update_merges_written_file_into_at_index() {
        let temp = tempfile::tempdir().unwrap();
        let new_file = temp.path().join("newly_created.rs");
        std::fs::write(&new_file, "fn main() {}\n").unwrap();
        let mut app = AppState::new();
        app.cwd = temp.path().to_string_lossy().to_string();

        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCallUpdate {
                tool_call_id: "write-1".into(),
                status: Some(ToolCallStatus::Completed),
                title: Some("Write newly_created.rs".into()),
                content: None,
                locations: None,
                raw_output: Some(write_raw_output(&new_file)),
            }),
        );

        let paths: Vec<_> = app
            .file_index
            .search("newly", 5)
            .into_iter()
            .map(|result| result.path)
            .collect();
        assert_eq!(paths, vec!["newly_created.rs".to_string()]);
    }

    #[test]
    fn completed_write_update_ignores_paths_outside_cwd_for_at_index() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("external.rs");
        std::fs::write(&outside_file, "fn main() {}\n").unwrap();
        let mut app = AppState::new();
        app.cwd = temp.path().to_string_lossy().to_string();

        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCallUpdate {
                tool_call_id: "write-1".into(),
                status: Some(ToolCallStatus::Completed),
                title: Some("Write external.rs".into()),
                content: None,
                locations: None,
                raw_output: Some(write_raw_output(&outside_file)),
            }),
        );

        assert!(app.file_index.search("external", 5).is_empty());
    }

    #[test]
    fn tool_call_update_patches_existing_streaming_tool_use() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCall {
                tool_call_id: "tool-1".into(),
                title: "Bash ls".into(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Pending,
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([("command".into(), json!("ls"))])),
                raw_output: None,
            }),
        );
        // While Pending the tool stays in the overlay — terminal
        // status hasn't been reached yet, so the progressive flush
        // can't seal it.
        assert!(app.rebon_tui.transcript.is_empty());
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);

        translate_session_update(
            &mut app,
            params(SessionUpdate::ToolCallUpdate {
                tool_call_id: "tool-1".into(),
                status: Some(ToolCallStatus::Completed),
                title: Some("Bash ls - completed".into()),
                content: None,
                locations: Some(vec![ToolCallLocation {
                    path: "Cargo.toml".into(),
                    line: Some(1),
                }]),
                raw_output: Some(raw_output(12)),
            }),
        );

        // The terminal-status update patches the overlay in place
        // but does NOT drain the tool to the transcript: the
        // sealed-prefix flush holds back trailing tool clusters so
        // that a sibling tool arriving later in the same turn can
        // group with it visually (instead of snapping into a
        // Collapsed group only after the second tool also flushes).
        assert!(
            app.rebon_tui.transcript.is_empty(),
            "trailing completed tool must stay in overlay so it can group with later sibling tools"
        );
        assert_eq!(app.rebon_tui.overlay.tool_use_count(), 1);
        let tool = app.rebon_tui.overlay.find_tool_use("tool-1").unwrap();
        assert_eq!(tool.call_id, "tool-1");
        assert_eq!(tool.tool_name, "Bash");
        assert_eq!(tool.status, ToolCallStatus::Completed);
        assert_eq!(tool.title.as_deref(), Some("Bash ls - completed"));
        assert_eq!(tool.locations.as_ref().unwrap()[0].path, "Cargo.toml");
        assert_eq!(tool.raw_output.as_ref().unwrap()["bytes"], json!(12));
        assert_eq!(tool.raw_input.as_ref().unwrap()["command"], json!("ls"));
    }

    #[test]
    fn compacting_done_restores_previous_spinner_verb() {
        let mut app = AppState::new();
        app.spinner_verb = "Cooking".into();

        translate_session_update(
            &mut app,
            params(SessionUpdate::CompactingStarted {
                messages_before: 42,
            }),
        );
        assert_eq!(app.spinner_verb, "Compacting");
        assert_eq!(
            app.spinner_verb_before_compacting.as_deref(),
            Some("Cooking")
        );

        translate_session_update(
            &mut app,
            params(SessionUpdate::CompactingDone {
                messages_after: 7,
                used_model: true,
            }),
        );

        assert_eq!(app.spinner_verb, "Cooking");
        assert_eq!(app.spinner_verb_before_compacting, None);
    }

    #[test]
    fn plan_update_stores_entries_on_app_state() {
        let mut app = AppState::new();
        let entries = vec![
            PlanEntry {
                content: "Implement feature A".into(),
                priority: PlanEntryPriority::High,
                status: PlanEntryStatus::InProgress,
            },
            PlanEntry {
                content: "Write tests".into(),
                priority: PlanEntryPriority::Medium,
                status: PlanEntryStatus::Pending,
            },
        ];
        translate_session_update(
            &mut app,
            params(SessionUpdate::Plan {
                entries: entries.clone(),
            }),
        );
        assert_eq!(app.plan_entries.len(), 2);
        assert_eq!(app.plan_entries[0].content, "Implement feature A");
        assert_eq!(app.plan_entries[0].status, PlanEntryStatus::InProgress);
        assert_eq!(app.plan_entries[1].content, "Write tests");
        // Transcript/overlay untouched
        assert!(app.rebon_tui.transcript.is_empty());
        assert!(app.rebon_tui.overlay.is_empty());
    }

    #[test]
    fn plan_update_replaces_previous_entries() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::Plan {
                entries: vec![PlanEntry {
                    content: "old".into(),
                    priority: PlanEntryPriority::Low,
                    status: PlanEntryStatus::Completed,
                }],
            }),
        );
        assert_eq!(app.plan_entries.len(), 1);

        translate_session_update(
            &mut app,
            params(SessionUpdate::Plan {
                entries: vec![
                    PlanEntry {
                        content: "new-1".into(),
                        priority: PlanEntryPriority::High,
                        status: PlanEntryStatus::Pending,
                    },
                    PlanEntry {
                        content: "new-2".into(),
                        priority: PlanEntryPriority::Medium,
                        status: PlanEntryStatus::Pending,
                    },
                ],
            }),
        );
        assert_eq!(app.plan_entries.len(), 2);
        assert_eq!(app.plan_entries[0].content, "new-1");
    }

    #[test]
    fn empty_plan_update_clears_entries() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::Plan {
                entries: vec![PlanEntry {
                    content: "task".into(),
                    priority: PlanEntryPriority::High,
                    status: PlanEntryStatus::Pending,
                }],
            }),
        );
        assert_eq!(app.plan_entries.len(), 1);

        translate_session_update(&mut app, params(SessionUpdate::Plan { entries: vec![] }));
        assert!(app.plan_entries.is_empty());
    }

    #[test]
    fn config_option_update_stores_options_on_app_state() {
        let mut app = AppState::new();
        let options = vec![ConfigOption {
            id: "permissions".into(),
            name: "Permissions".into(),
            description: Some("Tool permission mode".into()),
            category: None,
            option_type: ConfigOptionType::Select,
            current_value: "default".into(),
            options: vec![
                ConfigOptionValue {
                    value: "default".into(),
                    name: "Default".into(),
                    description: None,
                },
                ConfigOptionValue {
                    value: "plan".into(),
                    name: "Plan".into(),
                    description: None,
                },
            ],
        }];
        translate_session_update(
            &mut app,
            params(SessionUpdate::ConfigOptionUpdate {
                config_options: options,
            }),
        );
        assert_eq!(app.config_options.len(), 1);
        assert_eq!(app.config_options[0].id, "permissions");
        assert_eq!(app.config_options[0].current_value, "default");
        assert_eq!(app.config_options[0].options.len(), 2);
    }

    #[test]
    fn config_option_update_replaces_previous_options() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::ConfigOptionUpdate {
                config_options: vec![ConfigOption {
                    id: "old".into(),
                    name: "Old".into(),
                    description: None,
                    category: None,
                    option_type: ConfigOptionType::Select,
                    current_value: "v1".into(),
                    options: vec![],
                }],
            }),
        );
        assert_eq!(app.config_options.len(), 1);

        translate_session_update(
            &mut app,
            params(SessionUpdate::ConfigOptionUpdate {
                config_options: vec![
                    ConfigOption {
                        id: "new-a".into(),
                        name: "A".into(),
                        description: None,
                        category: None,
                        option_type: ConfigOptionType::Select,
                        current_value: "v2".into(),
                        options: vec![],
                    },
                    ConfigOption {
                        id: "new-b".into(),
                        name: "B".into(),
                        description: None,
                        category: None,
                        option_type: ConfigOptionType::Select,
                        current_value: "v3".into(),
                        options: vec![],
                    },
                ],
            }),
        );
        assert_eq!(app.config_options.len(), 2);
        assert_eq!(app.config_options[0].id, "new-a");
    }

    #[test]
    fn slash_commands_update_stores_commands_on_app_state() {
        let mut app = AppState::new();
        translate_session_update(
            &mut app,
            params(SessionUpdate::SlashCommands {
                commands: vec![rebon_types::SlashCommand {
                    name: "test".into(),
                    description: "A test".into(),
                    input: None,
                    category: None,
                    aliases: Vec::new(),
                }],
            }),
        );

        assert_eq!(app.slash_commands.len(), 1);
        assert_eq!(app.slash_commands[0].name, "test");
    }

    #[test]
    fn exit_plan_mode_completion_does_not_overwrite_selected_runtime_mode() {
        for mode in [PermissionMode::Auto, PermissionMode::AcceptEdits] {
            let mut app = AppState::new();
            app.set_permission_mode(mode);
            translate_session_update(
                &mut app,
                params(SessionUpdate::ToolCall {
                    tool_call_id: "exit-plan-1".into(),
                    title: "ExitPlanMode".into(),
                    kind: ToolKind::Other,
                    status: ToolCallStatus::InProgress,
                    content: None,
                    locations: None,
                    raw_input: Some(HashMap::from([("plan".into(), json!("do the work"))])),
                    raw_output: None,
                }),
            );
            translate_session_update(
                &mut app,
                params(SessionUpdate::ToolCallUpdate {
                    tool_call_id: "exit-plan-1".into(),
                    status: Some(ToolCallStatus::Completed),
                    title: Some("ExitPlanMode".into()),
                    content: None,
                    locations: None,
                    raw_output: Some(HashMap::from([(
                        "permissionMode".into(),
                        json!(mode.as_wire()),
                    )])),
                }),
            );

            assert_eq!(app.permission_mode, mode);
            assert!(!app.pending_plan_mode_tool_ids.contains_key("exit-plan-1"));
            assert!(app.hidden_tool_call_ids.contains("exit-plan-1"));
        }
    }

    #[test]
    fn context_reset_clears_transcript_and_marks_existing_ultraplan_executing() {
        let mut app = AppState::new();
        app.ultraplan_status = Some(crate::session::ultraplan_run::UltraplanStatus {
            run_id: "ultraplan-test".into(),
            phase: crate::session::ultraplan_run::UltraplanPhase::PlanModeActive,
            task_title: "build auth".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: None,
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        translate_session_update(
            &mut app,
            params(SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "planning output".into(),
                    annotations: None,
                }),
            }),
        );
        assert!(app.rebon_tui.overlay.combined_streaming_text().is_some());

        translate_session_update(&mut app, params(SessionUpdate::ContextReset { plan: None }));

        assert!(app.rebon_tui.transcript.is_empty());
        assert!(app.rebon_tui.overlay.combined_streaming_text().is_none());
        assert_eq!(
            app.ultraplan_status.as_ref().map(|status| status.phase),
            Some(crate::session::ultraplan_run::UltraplanPhase::Executing)
        );
    }

    #[test]
    fn context_reset_injects_plan_as_user_plan_content() {
        let mut app = AppState::new();
        let plan = "# Plan\n- edit code\n- run tests".to_string();

        translate_session_update(
            &mut app,
            params(SessionUpdate::ContextReset {
                plan: Some(plan.clone()),
            }),
        );

        assert_eq!(app.rebon_tui.transcript.len(), 1);
        match &app.rebon_tui.transcript.rows()[0] {
            rebon_tui::Message::User(user) => {
                assert_eq!(user.plan_content.as_deref(), Some(plan.as_str()));
                let first_text = user.message.content.first().and_then(|block| match block {
                    rebon_tui::UserContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                });
                assert_eq!(
                    first_text,
                    Some("Implement the following plan:\n\n# Plan\n- edit code\n- run tests")
                );
            }
            other => panic!("expected user plan message, got {other:?}"),
        }
    }

    #[test]
    fn session_info_update_stores_title_on_app_state() {
        let mut app = AppState::new();
        assert!(app.session_title.is_none());

        translate_session_update(
            &mut app,
            params(SessionUpdate::SessionInfoUpdate {
                title: Some("My Session".into()),
                updated_at: None,
                meta: None,
            }),
        );
        assert_eq!(app.session_title.as_deref(), Some("My Session"));
    }

    #[test]
    fn session_info_update_without_title_preserves_existing() {
        let mut app = AppState::new();
        app.session_title = Some("Previous Title".into());

        translate_session_update(
            &mut app,
            params(SessionUpdate::SessionInfoUpdate {
                title: None,
                updated_at: Some("2026-01-01T00:00:00Z".into()),
                meta: None,
            }),
        );
        // Title unchanged since the update had no title field
        assert_eq!(app.session_title.as_deref(), Some("Previous Title"));
    }

    #[test]
    fn session_info_update_replaces_existing_title() {
        let mut app = AppState::new();
        app.session_title = Some("Old Title".into());

        translate_session_update(
            &mut app,
            params(SessionUpdate::SessionInfoUpdate {
                title: Some("New Title".into()),
                updated_at: None,
                meta: None,
            }),
        );
        assert_eq!(app.session_title.as_deref(), Some("New Title"));
    }
}
