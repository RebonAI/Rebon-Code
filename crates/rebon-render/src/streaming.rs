//! Streaming overlay state: the in-flight content of a turn (text segments,
//! tool calls and thinking blocks) kept separate from the committed transcript
//! until the turn finishes.
//!
//! ## What the overlay holds
//!
//! ```text
//!   blocks              — `Vec<StreamingContentBlock>`, one entry per text
//!                         segment, tool use and thinking block, in arrival order
//!   live_shell_content  — full Bash/PowerShell payloads keyed by call id,
//!                         shared behind an `Arc` for the verbose view
//!   revision            — counter bumped by every mutating method, so a
//!                         caller can key a cache on the overlay's content
//! ```
//!
//! ## Ordered content blocks
//!
//! The overlay maintains an ordered `Vec<StreamingContentBlock>`
//! that interleaves text segments and tool calls in the order they
//! arrive, so interleaved text and tool cards keep their streaming order.
//! When new text arrives after a tool call, a new `Text` block is pushed
//! rather than appending to the previous text segment — this preserves the
//! chronological order:
//!
//! ```text
//! [Text("I'll read the file..."), ToolUse(Read), Text("The file says...")]
//! ```

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

fn strip_leading_system_reminders(text: &str) -> String {
    const CLOSE: &str = "</system-reminder>";
    let mut current = text.trim_start();
    while current.starts_with("<system-reminder>") {
        let Some(end) = current.find(CLOSE) else {
            break;
        };
        current = current[end + CLOSE.len()..].trim_start();
    }
    current.to_string()
}

use rebon_types::{
    ContentBlock, RegularContent, TextContent, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolKind,
};
use serde_json::Value;

pub const WORKFLOW_INTERRUPTED_MESSAGE: &str = "User intercepted workflow";

pub const LIVE_SHELL_PREVIEW_MAX_BYTES: usize = 32 * 1024;
pub const LIVE_SHELL_PREVIEW_MAX_LINES: usize = 128;
pub const LIVE_SHELL_PREVIEW_MAX_LINE_BYTES: usize = 4 * 1024;
pub const LIVE_SHELL_OMITTED_MARKER: &str = "… older live output omitted";
const LIVE_SHELL_STRUCTURED_PROGRESS_MARKER: &str = "[structured shell progress]";

pub fn is_live_shell_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "PowerShell")
}

fn is_terminal_status(status: ToolCallStatus) -> bool {
    matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
}

fn truncate_utf8_tail(line: &str, max_bytes: usize) -> (String, bool) {
    if line.len() <= max_bytes {
        return (line.to_string(), false);
    }

    const PREFIX: &str = "…";
    let suffix_budget = max_bytes.saturating_sub(PREFIX.len());
    let mut start = line.len().saturating_sub(suffix_budget);
    while start < line.len() && !line.is_char_boundary(start) {
        start += 1;
    }
    (format!("{PREFIX}{}", &line[start..]), true)
}

fn projection_size(lines: &VecDeque<String>, omitted: bool) -> usize {
    let line_bytes = lines.iter().map(String::len).sum::<usize>();
    let separators = lines.len().saturating_sub(1) + usize::from(omitted && !lines.is_empty());
    line_bytes
        .saturating_add(separators)
        .saturating_add(if omitted {
            LIVE_SHELL_OMITTED_MARKER.len()
        } else {
            0
        })
}

fn enforce_live_shell_projection_limits(lines: &mut VecDeque<String>, omitted: &mut bool) {
    loop {
        let line_limit = LIVE_SHELL_PREVIEW_MAX_LINES.saturating_sub(usize::from(*omitted));
        let over_lines = lines.len() > line_limit;
        let over_bytes = projection_size(lines, *omitted) > LIVE_SHELL_PREVIEW_MAX_BYTES;
        if !over_lines && !over_bytes {
            break;
        }
        if lines.pop_front().is_none() {
            break;
        }
        *omitted = true;
    }
}

fn push_live_shell_text(lines: &mut VecDeque<String>, omitted: &mut bool, text: &str) {
    let text = text.trim_end_matches(['\r', '\n']);
    if text.trim().is_empty() {
        return;
    }

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (line, truncated) = truncate_utf8_tail(line, LIVE_SHELL_PREVIEW_MAX_LINE_BYTES);
        *omitted |= truncated;
        lines.push_back(line);
        enforce_live_shell_projection_limits(lines, omitted);
    }
}

fn push_live_shell_content(
    lines: &mut VecDeque<String>,
    omitted: &mut bool,
    content: &[ToolCallContent],
) {
    for item in content {
        match item {
            ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(text),
            }) => push_live_shell_text(lines, omitted, &text.text),
            ToolCallContent::Content(_)
            | ToolCallContent::Diff(_)
            | ToolCallContent::Terminal(_) => {
                push_live_shell_text(lines, omitted, LIVE_SHELL_STRUCTURED_PROGRESS_MARKER);
            }
        }
    }
}

fn bounded_live_shell_content(
    existing: Option<&[ToolCallContent]>,
    incoming: &[ToolCallContent],
) -> Option<Vec<ToolCallContent>> {
    let mut lines = VecDeque::new();
    let mut omitted = false;

    if let Some(existing) = existing {
        for item in existing {
            match item {
                ToolCallContent::Content(RegularContent {
                    content: ContentBlock::Text(text),
                }) => {
                    for line in text.text.split('\n') {
                        if line == LIVE_SHELL_OMITTED_MARKER {
                            omitted = true;
                        } else {
                            lines.push_back(line.to_string());
                        }
                    }
                }
                ToolCallContent::Content(_)
                | ToolCallContent::Diff(_)
                | ToolCallContent::Terminal(_) => {
                    lines.push_back(LIVE_SHELL_STRUCTURED_PROGRESS_MARKER.to_string());
                }
            }
        }
    }

    push_live_shell_content(&mut lines, &mut omitted, incoming);
    enforce_live_shell_projection_limits(&mut lines, &mut omitted);
    if lines.is_empty() && !omitted {
        return None;
    }

    let mut text = String::with_capacity(projection_size(&lines, omitted));
    if omitted {
        text.push_str(LIVE_SHELL_OMITTED_MARKER);
    }
    for line in lines {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
    }
    Some(vec![ToolCallContent::Content(RegularContent {
        content: ContentBlock::Text(TextContent {
            text,
            annotations: None,
        }),
    })])
}

/// Streaming tool-use entry tracked by the overlay.
///
/// The local TUI receives tool-call lifecycle events from ACP, not the
/// final transcript `AssistantToolUseBlock`, so the overlay stores the
/// ACP-native fields directly and updates them with patch semantics.
#[derive(Debug, Clone)]
pub struct StreamingToolUse {
    /// Stable tool call id from ACP `toolCallId`.
    pub call_id: String,
    /// Canonical display name chosen by the CLI bridge (e.g. `Read`,
    /// `Bash`).
    pub tool_name: String,
    /// ACP tool kind, retained so renderers can branch on the real
    /// semantic category later.
    pub kind: ToolKind,
    /// Current lifecycle status.
    pub status: ToolCallStatus,
    /// Optional human-readable title from the update stream.
    pub title: Option<String>,
    /// Optional structured content payloads revealed during the call.
    pub content: Option<Vec<ToolCallContent>>,
    /// Optional file/line locations associated with the call.
    pub locations: Option<Vec<ToolCallLocation>>,
    /// Raw tool input map from the initial `tool_call` update.
    pub raw_input: Option<HashMap<String, Value>>,
    /// Raw tool output map from subsequent updates.
    pub raw_output: Option<HashMap<String, Value>>,
}

/// Extended-thinking block that is still arriving.
///
/// `thinking` carries the accumulated reasoning text (not a generic `text`
/// field), and `streaming_ended_at` an ISO-millis wall-clock stamp (not an
/// opaque monotonic tick). Renderers use `streaming_ended_at` to drive a 2s
/// fade-out after the stream closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingThinking {
    pub thinking: String,
    pub is_streaming: bool,
    /// ISO-millis wall clock of when the stream closed. Optional.
    pub streaming_ended_at: Option<u64>,
}

/// A single piece of streaming content, ordered chronologically.
#[derive(Debug, Clone)]
pub enum StreamingContentBlock {
    /// A text segment. Consecutive text deltas append to the same
    /// block. A new `Text` block starts after each `ToolUse`.
    Text(String),
    /// An in-flight tool use.
    ToolUse(StreamingToolUse),
    /// An extended-thinking block. Stored inline so it renders at
    /// its chronological position rather than always at the bottom.
    Thinking(StreamingThinking),
}

/// The parallel overlay that holds in-flight streaming fields.
///
/// Content blocks (text, tool uses, and thinking) are maintained in a
/// single ordered `Vec<StreamingContentBlock>` that preserves
/// chronological arrival order, so the renderer paints them in the order
/// they actually arrived.
#[derive(Debug, Clone, Default)]
pub struct StreamingOverlay {
    /// Ordered content blocks: text segments, tool calls, and thinking
    /// blocks in the order they were received.
    pub blocks: Vec<StreamingContentBlock>,
    /// True when `blocks[0]` is the live remainder of a text block whose
    /// top half was already committed to scrollback by the inline
    /// overflow flush. The renderer joins it to the committed half (no
    /// gutter dot, no leading margin) so the split reads as one message.
    /// Cleared whenever `blocks[0]` is consumed (drained or cleared).
    pub first_text_is_continuation: bool,
    /// Full in-progress Bash/PowerShell content. The visible tool block keeps a
    /// bounded projection so cloning the overlay for Compact/Normal rendering
    /// stays cheap; Verbose rendering reads this shared payload explicitly.
    live_shell_content: HashMap<String, Arc<Vec<ToolCallContent>>>,
    /// Bumped by every mutating method, so a caller can key a cache on the
    /// overlay's content without hashing it. Contract: any code that
    /// mutates the (public) `blocks` field directly instead of going
    /// through a method must be covered by another counter its cache key
    /// includes — the consumer's sealed-prefix flush and its finalize commit
    /// are both covered by `flush_counter` and `clear`.
    revision: u64,
}

fn is_workflow_progress_payload(raw_output: &HashMap<String, Value>) -> bool {
    raw_output.get("type").and_then(Value::as_str) == Some("workflow_progress")
}

/// True when a tool-use carries Workflow run state: the canonical
/// `Workflow` name, its `RunWorkflow` alias, or an `InvokeDeferredTool`
/// wrapper whose inner `tool_name` is one of those. Every site that
/// special-cases workflow tool uses by name (progress merging,
/// interrupt marking, card rendering) must use this predicate — matching
/// the literal `"Workflow"` silently drops the alias, which is exactly how
/// live progress entries once vanished from `RunWorkflow` cards.
pub fn is_workflow_tool_use(tool_name: &str, raw_input: Option<&HashMap<String, Value>>) -> bool {
    match tool_name {
        "Workflow" | "RunWorkflow" => true,
        "InvokeDeferredTool" => raw_input
            .and_then(|input| input.get("tool_name"))
            .and_then(Value::as_str)
            .is_some_and(|inner| matches!(inner, "Workflow" | "RunWorkflow")),
        _ => false,
    }
}

fn workflow_progress_sequence(event: &Value) -> Option<u64> {
    event.get("sequence").and_then(Value::as_u64)
}

fn append_workflow_progress_entry(progress: Option<Value>, event: Value) -> Value {
    let mut object = progress
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    if let Some(event_object) = event.as_object() {
        for key in ["runId", "workflowName", "summary"] {
            if let Some(value) = event_object.get(key).filter(|value| !value.is_null()) {
                object.insert(key.to_string(), value.clone());
            }
        }
    }

    let mut entries = object
        .remove("entries")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let duplicate = workflow_progress_sequence(&event).is_some_and(|sequence| {
        entries
            .iter()
            .any(|existing| workflow_progress_sequence(existing) == Some(sequence))
    });
    if !duplicate {
        entries.push(event);
    }
    object.insert("entries".into(), Value::Array(entries));
    Value::Object(object)
}

fn merge_workflow_progress_values(
    existing: Option<Value>,
    incoming: Option<Value>,
) -> Option<Value> {
    if existing.is_none() && incoming.is_none() {
        return None;
    }

    let mut object = existing
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let mut entries = object
        .remove("entries")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();

    if let Some(incoming_object) = incoming.as_ref().and_then(Value::as_object) {
        for key in ["runId", "workflowName", "summary"] {
            if let Some(value) = incoming_object.get(key).filter(|value| !value.is_null()) {
                object.insert(key.to_string(), value.clone());
            }
        }
        if let Some(incoming_entries) = incoming_object.get("entries").and_then(Value::as_array) {
            for event in incoming_entries {
                let duplicate = workflow_progress_sequence(event).is_some_and(|sequence| {
                    entries
                        .iter()
                        .any(|existing| workflow_progress_sequence(existing) == Some(sequence))
                });
                if !duplicate {
                    entries.push(event.clone());
                }
            }
        }
    }

    object.insert("entries".into(), Value::Array(entries));
    Some(Value::Object(object))
}

fn merge_workflow_raw_output(
    existing: Option<&HashMap<String, Value>>,
    mut incoming: HashMap<String, Value>,
) -> HashMap<String, Value> {
    if is_workflow_progress_payload(&incoming) {
        let event = Value::Object(incoming.into_iter().collect());
        let mut merged = existing.cloned().unwrap_or_default();
        let progress = append_workflow_progress_entry(merged.remove("workflowProgress"), event);
        merged.insert("workflowProgress".into(), progress);
        return merged;
    }

    let existing_progress = existing.and_then(|output| output.get("workflowProgress").cloned());
    let incoming_progress = incoming.remove("workflowProgress");
    if let Some(progress) = merge_workflow_progress_values(existing_progress, incoming_progress) {
        incoming.insert("workflowProgress".into(), progress);
    }
    incoming
}

impl StreamingOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_blocks(blocks: Vec<StreamingContentBlock>) -> Self {
        Self {
            blocks,
            ..Self::default()
        }
    }

    /// A counter that changes whenever a mutating method ran. Cache keys
    /// built from it must also include `AppState::flush_counter`: the
    /// sealed-prefix flush drains `blocks` directly and bumps that counter
    /// instead of this one.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Replace the current (last) text segment. If the last block is
    /// `Text`, replace it. Otherwise push a new `Text` block.
    pub fn set_streaming_text(&mut self, text: impl Into<String>) {
        self.bump_revision();
        let text = text.into();
        if let Some(StreamingContentBlock::Text(existing)) = self.blocks.last_mut() {
            *existing = text;
        } else {
            self.blocks.push(StreamingContentBlock::Text(text));
        }
    }

    /// Append a delta to the current text segment. If the last block
    /// is `Text`, append to it. Otherwise push a new `Text` block.
    /// This means text arriving after a tool call starts a NEW
    /// segment, preserving interleaved order.
    pub fn append_streaming_text(&mut self, delta: &str) {
        self.bump_revision();
        if let Some(StreamingContentBlock::Text(existing)) = self.blocks.last_mut() {
            existing.push_str(delta);
        } else {
            self.blocks
                .push(StreamingContentBlock::Text(delta.to_string()));
        }
    }

    /// Add or replace a streaming tool-use. Keyed on the ACP
    /// `toolCallId`. When inserting a new tool use, it is appended
    /// as a new block — any subsequent text will start a fresh
    /// `Text` block after it.
    pub fn upsert_streaming_tool_use(&mut self, mut tool_use: StreamingToolUse) {
        self.bump_revision();
        let call_id = tool_use.call_id.clone();
        if is_live_shell_tool(&tool_use.tool_name) && !is_terminal_status(tool_use.status) {
            let full_content = tool_use.content.take().unwrap_or_default();
            tool_use.content = bounded_live_shell_content(None, &full_content);
            self.live_shell_content
                .insert(call_id.clone(), Arc::new(full_content));
        } else {
            self.live_shell_content.remove(&call_id);
        }

        // Check if this call_id already exists in blocks.
        for block in self.blocks.iter_mut() {
            if let StreamingContentBlock::ToolUse(existing) = block {
                if existing.call_id == call_id {
                    *existing = tool_use;
                    return;
                }
            }
        }
        self.blocks.push(StreamingContentBlock::ToolUse(tool_use));
    }

    /// Patch a streaming tool-use in place. Only fields present in the
    /// update are overwritten.
    ///
    /// Returns `false` when `call_id` matches no overlay block — the
    /// update is dropped in that case, so a `Completed` status that
    /// misses leaves the block (if it exists elsewhere) permanently
    /// non-terminal. Callers should surface a miss rather than swallow it.
    pub fn update_streaming_tool_use(
        &mut self,
        call_id: &str,
        status: Option<ToolCallStatus>,
        title: Option<String>,
        content: Option<Vec<ToolCallContent>>,
        locations: Option<Vec<ToolCallLocation>>,
        raw_output: Option<HashMap<String, Value>>,
    ) -> bool {
        let Some(block_index) = self.blocks.iter().position(
            |block| matches!(block, StreamingContentBlock::ToolUse(tool) if tool.call_id == call_id),
        ) else {
            return false;
        };
        self.bump_revision();
        let (tool_name, current_status) = match &self.blocks[block_index] {
            StreamingContentBlock::ToolUse(tool) => (tool.tool_name.clone(), tool.status),
            _ => unreachable!("tool position matched a non-tool block"),
        };
        let effective_status = status.unwrap_or(current_status);
        let live_shell = is_live_shell_tool(&tool_name) && !is_terminal_status(effective_status);
        let terminal_shell_update =
            is_live_shell_tool(&tool_name) && status.is_some_and(is_terminal_status);

        let mut replace_content: Option<Option<Vec<ToolCallContent>>> = None;
        let mut append_content = None;
        if live_shell {
            if let Some(incoming) = content {
                let existing_projection = match &mut self.blocks[block_index] {
                    StreamingContentBlock::ToolUse(tool) => tool.content.take(),
                    _ => unreachable!("tool position matched a non-tool block"),
                };
                let projection =
                    bounded_live_shell_content(existing_projection.as_deref(), &incoming);
                let full_content = self
                    .live_shell_content
                    .entry(call_id.to_string())
                    .or_insert_with(|| Arc::new(existing_projection.unwrap_or_default()));
                Arc::make_mut(full_content).extend(incoming);
                replace_content = Some(projection);
            }
        } else if terminal_shell_update {
            let full_content = self.live_shell_content.remove(call_id);
            if let Some(content) = content {
                replace_content = Some(Some(content));
            } else if let Some(full_content) = full_content {
                let terminal_content = if raw_output.is_none() {
                    Some(Arc::try_unwrap(full_content).unwrap_or_else(|shared| (*shared).clone()))
                } else {
                    None
                };
                replace_content = Some(terminal_content);
            }
        } else {
            append_content = content;
        }

        let existing = match &mut self.blocks[block_index] {
            StreamingContentBlock::ToolUse(tool) => tool,
            _ => unreachable!("tool position matched a non-tool block"),
        };
        if let Some(status) = status {
            existing.status = status;
        }
        if let Some(title) = title {
            existing.title = Some(title);
        }
        if let Some(content) = replace_content {
            existing.content = content;
        } else if let Some(content) = append_content {
            if let Some(existing_content) = &mut existing.content {
                existing_content.extend(content);
            } else {
                existing.content = Some(content);
            }
        }
        if let Some(locations) = locations {
            existing.locations = Some(locations);
        }
        if let Some(raw_output) = raw_output {
            if is_workflow_tool_use(&existing.tool_name, existing.raw_input.as_ref()) {
                let merged = merge_workflow_raw_output(existing.raw_output.as_ref(), raw_output);
                existing.raw_output = Some(merged);
            } else if existing.tool_name == "Agent" {
                if let Some(existing_raw_output) = existing.raw_output.as_ref() {
                    let render_extras: HashMap<String, Value> = existing_raw_output
                        .iter()
                        .filter(|(key, _)| {
                            matches!(key.as_str(), "sub_agent_tool_calls" | "subAgentToolCalls")
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    let mut merged = raw_output;
                    for (key, value) in render_extras {
                        merged.entry(key).or_insert(value);
                    }
                    existing.raw_output = Some(merged);
                } else {
                    existing.raw_output = Some(raw_output);
                }
            } else {
                existing.raw_output = Some(raw_output);
            }
        };
        true
    }

    pub fn materialize_full_live_shell_content(&mut self) {
        self.bump_revision();
        let mut full_content = std::mem::take(&mut self.live_shell_content);
        for block in &mut self.blocks {
            let StreamingContentBlock::ToolUse(tool) = block else {
                continue;
            };
            let Some(content) = full_content.remove(&tool.call_id) else {
                continue;
            };
            tool.content =
                Some(Arc::try_unwrap(content).unwrap_or_else(|shared| (*shared).clone()));
        }
    }

    /// Remove a streaming tool-use by ACP `toolCallId`.
    pub fn remove_streaming_tool_use(&mut self, call_id: &str) {
        self.bump_revision();
        self.blocks.retain(
            |block| !matches!(block, StreamingContentBlock::ToolUse(t) if t.call_id == call_id),
        );
        self.live_shell_content.remove(call_id);
    }

    /// Reset the whole overlay — called once a turn finishes or is
    /// cancelled, so the next turn starts from an empty block list.
    pub fn clear(&mut self) {
        self.bump_revision();
        self.blocks.clear();
        self.first_text_is_continuation = false;
        self.live_shell_content.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Append a delta to the current thinking block. If the last
    /// `Thinking` block is still streaming, append to it. Otherwise
    /// push a new `Thinking` block. This preserves chronological
    /// ordering — thinking arrives before text in a turn.
    pub fn append_streaming_thinking(&mut self, delta: &str) {
        self.bump_revision();
        // Find the last thinking block that is still streaming.
        if let Some(StreamingContentBlock::Thinking(existing)) = self.blocks.last_mut() {
            if existing.is_streaming {
                existing.thinking.push_str(delta);
                return;
            }
        }
        // No active thinking block — start a new one.
        self.blocks
            .push(StreamingContentBlock::Thinking(StreamingThinking {
                thinking: delta.to_string(),
                is_streaming: true,
                streaming_ended_at: None,
            }));
    }

    /// Mark the current thinking block as finished.
    pub fn end_streaming_thinking(&mut self) {
        self.bump_revision();
        // Walk backwards to find the last active thinking block.
        for block in self.blocks.iter_mut().rev() {
            if let StreamingContentBlock::Thinking(t) = block {
                if t.is_streaming {
                    t.is_streaming = false;
                    t.streaming_ended_at = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                    );
                    return;
                }
            }
        }
    }

    /// Defensively close EVERY thinking block still marked streaming,
    /// including ones buried mid-overlay. `end_streaming_thinking`
    /// (the `ThinkingEnd` event handler) only closes the most recent
    /// streaming block, so a lost event leaves an earlier block
    /// `is_streaming` forever — and the inline sealed-prefix flush
    /// breaks its forward walk on the first open thinking block,
    /// stranding everything after it in the live overlay until the
    /// turn finalizes. Call this when a new visible block (text delta,
    /// tool use) enters the overlay: provider streams emit blocks
    /// strictly in order, so new visible content means no reasoning
    /// stream can still be open. Idempotent; a no-op when nothing is
    /// streaming.
    pub fn close_all_streaming_thinking(&mut self) {
        self.bump_revision();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        for block in self.blocks.iter_mut() {
            if let StreamingContentBlock::Thinking(t) = block {
                if t.is_streaming {
                    t.is_streaming = false;
                    t.streaming_ended_at = Some(now_ms);
                }
            }
        }
    }

    /// Read-only accessor for the last thinking block (if any).
    pub fn streaming_thinking(&self) -> Option<&StreamingThinking> {
        self.blocks.iter().rev().find_map(|b| match b {
            StreamingContentBlock::Thinking(t) => Some(t),
            _ => None,
        })
    }

    /// True when at least one text block has non-whitespace content.
    /// Whitespace-only partials do NOT count as
    /// visible content, so the cancel-commit reducer will drop them.
    pub fn streaming_text_has_visible_content(&self) -> bool {
        self.blocks.iter().any(
            |block| matches!(block, StreamingContentBlock::Text(text) if !text.trim().is_empty()),
        )
    }

    /// Concatenate all text segments into a single string.
    /// Used by the cancel-commit path to build the committed
    /// assistant message.
    pub fn combined_streaming_text(&self) -> Option<String> {
        let mut combined = String::new();
        for block in &self.blocks {
            if let StreamingContentBlock::Text(text) = block {
                combined.push_str(text);
            }
        }
        let stripped = strip_leading_system_reminders(&combined);
        // Leaked side-channel summary envelopes (`{"summary":"…"}`)
        // read better as their summary sentence than as raw JSON.
        let stripped = rebon_types::display_text_for_summary_envelope(&stripped);
        if stripped.is_empty() {
            None
        } else {
            Some(stripped)
        }
    }

    /// Find a tool use by call_id (immutable).
    pub fn find_tool_use(&self, call_id: &str) -> Option<&StreamingToolUse> {
        self.blocks.iter().find_map(|block| match block {
            StreamingContentBlock::ToolUse(t) if t.call_id == call_id => Some(t),
            _ => None,
        })
    }

    /// Full live shell content retained for an explicit Verbose render. Compact
    /// and Normal callers should use `StreamingToolUse::content`, which is the
    /// bounded projection.
    pub fn full_live_shell_content(&self, call_id: &str) -> Option<&[ToolCallContent]> {
        self.live_shell_content
            .get(call_id)
            .map(|content| content.as_slice())
    }

    /// Count of tool use blocks.
    pub fn tool_use_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|b| matches!(b, StreamingContentBlock::ToolUse(_)))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::{ContentBlock, RegularContent};

    fn fake_tool_use(call_id: &str, tool_name: &str) -> StreamingToolUse {
        let mut raw_input = HashMap::new();
        raw_input.insert("path".into(), Value::String("a.txt".into()));
        StreamingToolUse {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::Pending,
            title: Some(format!("{tool_name} a.txt")),
            content: None,
            locations: None,
            raw_input: Some(raw_input),
            raw_output: None,
        }
    }

    fn text_tool_content(text: impl Into<String>) -> ToolCallContent {
        ToolCallContent::Content(RegularContent {
            content: ContentBlock::Text(TextContent {
                text: text.into(),
                annotations: None,
            }),
        })
    }

    fn projected_text(tool: &StreamingToolUse) -> &str {
        let Some(ToolCallContent::Content(RegularContent {
            content: ContentBlock::Text(text),
        })) = tool.content.as_ref().and_then(|content| content.first())
        else {
            panic!("expected text projection");
        };
        &text.text
    }

    #[test]
    fn new_overlay_is_empty() {
        let o = StreamingOverlay::new();
        assert!(o.is_empty());
        assert!(!o.streaming_text_has_visible_content());
    }

    #[test]
    fn set_and_append_streaming_text() {
        let mut o = StreamingOverlay::new();
        o.set_streaming_text("hello");
        assert_eq!(o.combined_streaming_text().as_deref(), Some("hello"));
        o.append_streaming_text(" world");
        assert_eq!(o.combined_streaming_text().as_deref(), Some("hello world"));
    }

    #[test]
    fn append_to_empty_initializes() {
        let mut o = StreamingOverlay::new();
        o.append_streaming_text("first");
        assert_eq!(o.combined_streaming_text().as_deref(), Some("first"));
    }

    #[test]
    fn text_after_tool_use_starts_new_segment() {
        let mut o = StreamingOverlay::new();
        o.append_streaming_text("before tool");
        o.upsert_streaming_tool_use(fake_tool_use("t1", "Read"));
        o.append_streaming_text("after tool");

        assert_eq!(o.blocks.len(), 3);
        assert!(matches!(&o.blocks[0], StreamingContentBlock::Text(t) if t == "before tool"));
        assert!(matches!(&o.blocks[1], StreamingContentBlock::ToolUse(t) if t.call_id == "t1"));
        assert!(matches!(&o.blocks[2], StreamingContentBlock::Text(t) if t == "after tool"));
        assert_eq!(
            o.combined_streaming_text().as_deref(),
            Some("before toolafter tool")
        );
    }

    #[test]
    fn consecutive_text_appends_accumulate_in_same_block() {
        let mut o = StreamingOverlay::new();
        o.append_streaming_text("a");
        o.append_streaming_text("b");
        o.append_streaming_text("c");
        assert_eq!(o.blocks.len(), 1);
        assert!(matches!(&o.blocks[0], StreamingContentBlock::Text(t) if t == "abc"));
    }

    #[test]
    fn close_all_streaming_thinking_closes_buried_leaked_blocks() {
        // A ThinkingEnd that never arrived (message ended on a thinking
        // block, stream retry) leaves an is_streaming block mid-overlay;
        // end_streaming_thinking only closes the LAST streaming block,
        // so the leaked one blocks the sealed-prefix walk forever.
        let mut o = StreamingOverlay::new();
        o.append_streaming_thinking("leaked reasoning");
        o.upsert_streaming_tool_use(fake_tool_use("t1", "Read"));
        o.append_streaming_thinking("current reasoning");
        assert!(matches!(
            &o.blocks[0],
            StreamingContentBlock::Thinking(t) if t.is_streaming
        ));
        assert!(matches!(
            &o.blocks[2],
            StreamingContentBlock::Thinking(t) if t.is_streaming
        ));

        // end_streaming_thinking only reaches the most recent block.
        o.end_streaming_thinking();
        assert!(matches!(
            &o.blocks[0],
            StreamingContentBlock::Thinking(t) if t.is_streaming
        ));
        assert!(matches!(
            &o.blocks[2],
            StreamingContentBlock::Thinking(t) if !t.is_streaming
        ));

        o.append_streaming_thinking("more");
        o.close_all_streaming_thinking();
        for block in &o.blocks {
            if let StreamingContentBlock::Thinking(t) = block {
                assert!(!t.is_streaming, "all thinking blocks must be closed");
                assert!(t.streaming_ended_at.is_some());
            }
        }
    }

    #[test]
    fn close_all_streaming_thinking_is_noop_without_open_blocks() {
        let mut o = StreamingOverlay::new();
        o.append_streaming_text("text only");
        o.close_all_streaming_thinking();
        assert_eq!(o.blocks.len(), 1);

        o.append_streaming_thinking("think");
        o.end_streaming_thinking();
        let ended_at = match &o.blocks[1] {
            StreamingContentBlock::Thinking(t) => t.streaming_ended_at,
            other => panic!("expected thinking block, got {other:?}"),
        };
        o.close_all_streaming_thinking();
        // Already-closed blocks keep their original end timestamp.
        assert!(matches!(
            &o.blocks[1],
            StreamingContentBlock::Thinking(t)
                if !t.is_streaming && t.streaming_ended_at == ended_at
        ));
    }

    /// The revision is what render-side caches key on instead of hashing
    /// the overlay. Every mutating method must bump it — a method that
    /// forgets leaves a stale height on screen until unrelated content
    /// moves. New mutators belong in this list.
    #[test]
    fn every_mutating_method_bumps_the_revision() {
        let mut o = StreamingOverlay::new();
        let mut last = o.revision();
        let bumped = |o: &StreamingOverlay, what: &str, last: &mut u64| {
            assert!(o.revision() != *last, "{what} must bump the revision");
            *last = o.revision();
        };
        o.set_streaming_text("a");
        bumped(&o, "set_streaming_text", &mut last);
        o.append_streaming_text("b");
        bumped(&o, "append_streaming_text", &mut last);
        o.append_streaming_thinking("t");
        bumped(&o, "append_streaming_thinking", &mut last);
        o.end_streaming_thinking();
        bumped(&o, "end_streaming_thinking", &mut last);
        o.close_all_streaming_thinking();
        bumped(&o, "close_all_streaming_thinking", &mut last);
        o.upsert_streaming_tool_use(fake_tool_use("toolu_rev", "Read"));
        bumped(&o, "upsert_streaming_tool_use", &mut last);
        assert!(o.update_streaming_tool_use(
            "toolu_rev",
            Some(ToolCallStatus::Completed),
            None,
            None,
            None,
            None,
        ));
        bumped(&o, "update_streaming_tool_use", &mut last);
        o.materialize_full_live_shell_content();
        bumped(&o, "materialize_full_live_shell_content", &mut last);
        o.remove_streaming_tool_use("toolu_rev");
        bumped(&o, "remove_streaming_tool_use", &mut last);
        o.clear();
        bumped(&o, "clear", &mut last);
    }

    #[test]
    fn update_streaming_tool_use_reports_hit_and_miss() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("toolu_1", "Read"));

        assert!(o.update_streaming_tool_use(
            "toolu_1",
            Some(ToolCallStatus::Completed),
            None,
            None,
            None,
            None,
        ));
        assert!(
            !o.update_streaming_tool_use(
                "toolu_missing",
                Some(ToolCallStatus::Completed),
                None,
                None,
                None,
                None,
            ),
            "a miss must be reported so callers can log the dropped update"
        );
        assert_eq!(
            o.find_tool_use("toolu_1").unwrap().status,
            ToolCallStatus::Completed
        );
    }

    #[test]
    fn upsert_streaming_tool_use_keyed_by_call_id() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("toolu_1", "Read"));
        o.upsert_streaming_tool_use(fake_tool_use("toolu_2", "Bash"));

        let mut updated = fake_tool_use("toolu_1", "Read");
        updated.status = ToolCallStatus::InProgress;
        o.upsert_streaming_tool_use(updated);

        assert_eq!(o.tool_use_count(), 2);
        let t1 = o.find_tool_use("toolu_1").unwrap();
        assert_eq!(t1.status, ToolCallStatus::InProgress);
        let t2 = o.find_tool_use("toolu_2").unwrap();
        assert_eq!(t2.status, ToolCallStatus::Pending);
    }

    #[test]
    fn update_streaming_tool_use_patches_optional_fields() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("toolu_1", "Read"));

        o.update_streaming_tool_use(
            "toolu_1",
            Some(ToolCallStatus::Completed),
            Some("Read Cargo.toml".into()),
            Some(vec![ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "done".into(),
                    annotations: None,
                }),
            })]),
            Some(vec![ToolCallLocation {
                path: "Cargo.toml".into(),
                line: Some(1),
            }]),
            Some(HashMap::from([("size".into(), Value::Number(123.into()))])),
        );

        let tool = o.find_tool_use("toolu_1").unwrap();
        assert_eq!(tool.status, ToolCallStatus::Completed);
        assert_eq!(tool.title.as_deref(), Some("Read Cargo.toml"));
        assert!(tool.content.is_some());
        assert_eq!(tool.locations.as_ref().unwrap()[0].path, "Cargo.toml");
        assert_eq!(
            tool.raw_input.as_ref().unwrap()["path"],
            Value::String("a.txt".into())
        );
        assert_eq!(
            tool.raw_output.as_ref().unwrap()["size"],
            Value::Number(123.into())
        );
    }

    #[test]
    fn update_streaming_tool_use_appends_content_updates() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("toolu_1", "Agent"));

        o.update_streaming_tool_use(
            "toolu_1",
            None,
            None,
            Some(vec![ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "first".into(),
                    annotations: None,
                }),
            })]),
            None,
            None,
        );
        o.update_streaming_tool_use(
            "toolu_1",
            None,
            None,
            Some(vec![ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "second".into(),
                    annotations: None,
                }),
            })]),
            None,
            None,
        );

        let tool = o.find_tool_use("toolu_1").unwrap();
        assert_eq!(tool.content.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn live_shell_projection_is_bounded_while_verbose_content_stays_complete() {
        for tool_name in ["Bash", "PowerShell"] {
            let mut overlay = StreamingOverlay::new();
            let mut tool = fake_tool_use("shell-1", tool_name);
            tool.kind = ToolKind::Execute;
            tool.status = ToolCallStatus::InProgress;
            overlay.upsert_streaming_tool_use(tool);

            for index in 0..10_000 {
                assert!(overlay.update_streaming_tool_use(
                    "shell-1",
                    Some(ToolCallStatus::InProgress),
                    None,
                    Some(vec![text_tool_content(format!(
                        "line-{index:05}-{}",
                        "x".repeat(32)
                    ))]),
                    None,
                    None,
                ));
            }

            let projection = projected_text(overlay.find_tool_use("shell-1").unwrap());
            assert!(projection.len() <= LIVE_SHELL_PREVIEW_MAX_BYTES);
            assert!(projection.lines().count() <= LIVE_SHELL_PREVIEW_MAX_LINES);
            assert!(projection.starts_with(LIVE_SHELL_OMITTED_MARKER));
            assert!(!projection.contains("line-00000-"));
            assert!(projection.contains("line-09999-"));
            assert_eq!(projection.matches(LIVE_SHELL_OMITTED_MARKER).count(), 1);

            let full = overlay
                .full_live_shell_content("shell-1")
                .expect("full live shell content");
            assert_eq!(full.len(), 10_000);
            let full_ptr = full.as_ptr();
            let cloned = overlay.clone();
            assert_eq!(
                cloned
                    .full_live_shell_content("shell-1")
                    .expect("cloned full content")
                    .as_ptr(),
                full_ptr,
                "overlay clones must share the full live payload"
            );
        }
    }

    #[test]
    fn materializing_live_shell_content_restores_full_history() {
        let mut overlay = StreamingOverlay::new();
        let mut tool = fake_tool_use("shell-1", "PowerShell");
        tool.kind = ToolKind::Execute;
        tool.status = ToolCallStatus::InProgress;
        overlay.upsert_streaming_tool_use(tool);

        for index in 0..1_000 {
            assert!(overlay.update_streaming_tool_use(
                "shell-1",
                Some(ToolCallStatus::InProgress),
                None,
                Some(vec![text_tool_content(format!("line-{index:04}"))]),
                None,
                None,
            ));
        }
        assert!(!projected_text(overlay.find_tool_use("shell-1").unwrap()).contains("line-0000"));

        overlay.materialize_full_live_shell_content();

        assert!(overlay.full_live_shell_content("shell-1").is_none());
        let content = overlay
            .find_tool_use("shell-1")
            .and_then(|tool| tool.content.as_ref())
            .expect("materialized shell content");
        assert_eq!(content.len(), 1_000);
        assert!(matches!(
            &content[0],
            ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(text),
            }) if text.text == "line-0000"
        ));
        assert!(matches!(
            &content[999],
            ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(text),
            }) if text.text == "line-0999"
        ));
    }

    #[test]
    fn live_shell_projection_clips_long_utf8_lines_on_character_boundaries() {
        let mut overlay = StreamingOverlay::new();
        let mut tool = fake_tool_use("shell-1", "PowerShell");
        tool.kind = ToolKind::Execute;
        tool.status = ToolCallStatus::InProgress;
        overlay.upsert_streaming_tool_use(tool);
        let output = format!("HEAD-{}-TAIL", "你🙂".repeat(20_000));

        overlay.update_streaming_tool_use(
            "shell-1",
            None,
            None,
            Some(vec![text_tool_content(output.clone())]),
            None,
            None,
        );

        let projection = projected_text(overlay.find_tool_use("shell-1").unwrap());
        assert!(projection.starts_with(LIVE_SHELL_OMITTED_MARKER));
        let retained = projection.lines().last().expect("retained line");
        assert!(retained.len() <= LIVE_SHELL_PREVIEW_MAX_LINE_BYTES);
        assert!(retained.ends_with("-TAIL"));
        assert!(std::str::from_utf8(projection.as_bytes()).is_ok());
        let full = overlay.full_live_shell_content("shell-1").unwrap();
        let ToolCallContent::Content(RegularContent {
            content: ContentBlock::Text(text),
        }) = &full[0]
        else {
            panic!("expected text content");
        };
        assert_eq!(text.text, output);
    }

    #[test]
    fn live_shell_structured_progress_uses_a_fixed_projection_placeholder() {
        let mut overlay = StreamingOverlay::new();
        let mut tool = fake_tool_use("shell-1", "Bash");
        tool.kind = ToolKind::Execute;
        tool.status = ToolCallStatus::InProgress;
        overlay.upsert_streaming_tool_use(tool);

        overlay.update_streaming_tool_use(
            "shell-1",
            None,
            None,
            Some(vec![ToolCallContent::Terminal(
                rebon_types::TerminalContent {
                    terminal_id: "terminal-1".into(),
                },
            )]),
            None,
            None,
        );

        assert_eq!(
            projected_text(overlay.find_tool_use("shell-1").unwrap()),
            LIVE_SHELL_STRUCTURED_PROGRESS_MARKER
        );
        assert!(matches!(
            overlay.full_live_shell_content("shell-1").unwrap(),
            [ToolCallContent::Terminal(_)]
        ));
    }

    #[test]
    fn terminal_shell_updates_drop_live_sidecar_and_use_final_content() {
        for status in [ToolCallStatus::Completed, ToolCallStatus::Failed] {
            let mut overlay = StreamingOverlay::new();
            let mut tool = fake_tool_use("shell-1", "Bash");
            tool.kind = ToolKind::Execute;
            tool.status = ToolCallStatus::InProgress;
            overlay.upsert_streaming_tool_use(tool);
            overlay.update_streaming_tool_use(
                "shell-1",
                None,
                None,
                Some(vec![text_tool_content("live output")]),
                None,
                None,
            );

            overlay.update_streaming_tool_use(
                "shell-1",
                Some(status),
                None,
                Some(vec![text_tool_content("final output")]),
                None,
                Some(HashMap::from([(
                    "stdout".into(),
                    Value::String("final stdout".into()),
                )])),
            );

            let tool = overlay.find_tool_use("shell-1").unwrap();
            assert_eq!(tool.status, status);
            assert_eq!(projected_text(tool), "final output");
            assert!(overlay.full_live_shell_content("shell-1").is_none());
        }
    }

    #[test]
    fn repeated_terminal_shell_patches_preserve_final_content() {
        for tool_name in ["Bash", "PowerShell"] {
            for status in [ToolCallStatus::Completed, ToolCallStatus::Failed] {
                let mut overlay = StreamingOverlay::new();
                let mut tool = fake_tool_use("shell-1", tool_name);
                tool.kind = ToolKind::Execute;
                tool.status = ToolCallStatus::InProgress;
                overlay.upsert_streaming_tool_use(tool);
                overlay.update_streaming_tool_use(
                    "shell-1",
                    None,
                    None,
                    Some(vec![text_tool_content("live output")]),
                    None,
                    None,
                );
                overlay.update_streaming_tool_use(
                    "shell-1",
                    Some(status),
                    None,
                    Some(vec![text_tool_content("final output")]),
                    None,
                    Some(HashMap::from([(
                        "stdout".into(),
                        Value::String("final stdout".into()),
                    )])),
                );

                overlay.update_streaming_tool_use("shell-1", Some(status), None, None, None, None);
                assert_eq!(
                    projected_text(overlay.find_tool_use("shell-1").unwrap()),
                    "final output"
                );

                overlay.update_streaming_tool_use(
                    "shell-1",
                    Some(status),
                    None,
                    None,
                    None,
                    Some(HashMap::from([(
                        "stdout".into(),
                        Value::String("refreshed stdout".into()),
                    )])),
                );
                let tool = overlay.find_tool_use("shell-1").unwrap();
                assert_eq!(projected_text(tool), "final output");
                assert_eq!(
                    tool.raw_output
                        .as_ref()
                        .and_then(|output| output.get("stdout"))
                        .and_then(Value::as_str),
                    Some("refreshed stdout")
                );
            }
        }
    }

    #[test]
    fn final_agent_raw_output_update_preserves_sub_agent_tool_call_history() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("agent-1", "Agent"));

        let snake_history = Value::Array(vec![
            serde_json::json!({ "tool_use_id": "read-1", "name": "Read", "ok": true }),
            serde_json::json!({ "tool_use_id": "grep-1", "name": "Grep", "ok": true }),
        ]);
        let camel_history = Value::Array(vec![
            serde_json::json!({ "toolUseID": "read-2", "name": "Read", "ok": true }),
        ]);
        o.update_streaming_tool_use(
            "agent-1",
            None,
            None,
            None,
            None,
            Some(HashMap::from([
                ("sub_agent_tool_calls".into(), snake_history.clone()),
                ("subAgentToolCalls".into(), camel_history.clone()),
                (
                    "activity_lines".into(),
                    serde_json::json!(["/workspace/src/lib.rs"]),
                ),
            ])),
        );

        o.update_streaming_tool_use(
            "agent-1",
            Some(ToolCallStatus::Completed),
            None,
            Some(vec![ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "final answer".into(),
                    annotations: None,
                }),
            })]),
            None,
            Some(HashMap::from([
                ("status".into(), Value::String("completed".into())),
                ("final_text".into(), Value::String("final answer".into())),
                (
                    "usage".into(),
                    serde_json::json!({ "input_tokens": 7, "output_tokens": 3 }),
                ),
            ])),
        );

        let tool = o.find_tool_use("agent-1").unwrap();
        let raw_output = tool.raw_output.as_ref().unwrap();
        assert_eq!(raw_output.get("sub_agent_tool_calls"), Some(&snake_history));
        assert_eq!(raw_output.get("subAgentToolCalls"), Some(&camel_history));
        assert_eq!(
            raw_output.get("final_text").and_then(Value::as_str),
            Some("final answer")
        );
        assert_eq!(tool.content.as_ref().map(Vec::len), Some(1));
        assert!(!raw_output.contains_key("activity_lines"));
    }

    #[test]
    fn workflow_progress_raw_output_updates_append_and_dedupe_entries() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("wf-1", "Workflow"));

        let progress = |sequence: u64, entry: Value| {
            HashMap::from([
                ("type".into(), Value::String("workflow_progress".into())),
                ("runId".into(), Value::String("wf_test".into())),
                ("workflowName".into(), Value::String("previewer".into())),
                ("sequence".into(), Value::Number(sequence.into())),
                ("entry".into(), entry),
            ])
        };
        o.update_streaming_tool_use(
            "wf-1",
            None,
            None,
            None,
            None,
            Some(progress(
                1,
                serde_json::json!({ "type": "phase", "title": "design", "state": "start" }),
            )),
        );
        o.update_streaming_tool_use(
            "wf-1",
            None,
            None,
            None,
            None,
            Some(progress(
                2,
                serde_json::json!({ "type": "agent", "index": 1, "state": "start", "phaseTitle": "design", "label": "ui" }),
            )),
        );
        o.update_streaming_tool_use(
            "wf-1",
            None,
            None,
            None,
            None,
            Some(progress(
                2,
                serde_json::json!({ "type": "agent", "index": 1, "state": "start", "phaseTitle": "design", "label": "ui" }),
            )),
        );

        let tool = o.find_tool_use("wf-1").unwrap();
        let progress = tool
            .raw_output
            .as_ref()
            .unwrap()
            .get("workflowProgress")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            progress.get("workflowName").and_then(Value::as_str),
            Some("previewer")
        );
        assert_eq!(
            progress
                .get("entries")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
    }

    /// The engine registers the workflow tool under the `RunWorkflow` alias;
    /// progress payloads must accumulate for it exactly as for `Workflow`.
    /// (Regression: the merge gate matched the literal `"Workflow"`, so every
    /// alias progress event REPLACED raw_output and the live card stayed
    /// stuck at the launch ack line.)
    #[test]
    fn workflow_progress_merges_for_run_workflow_alias() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("wf-alias", "RunWorkflow"));

        let progress = |sequence: u64, entry: Value| {
            HashMap::from([
                ("type".into(), Value::String("workflow_progress".into())),
                ("runId".into(), Value::String("wf_alias".into())),
                ("workflowName".into(), Value::String("aliased".into())),
                ("sequence".into(), Value::Number(sequence.into())),
                ("entry".into(), entry),
            ])
        };
        o.update_streaming_tool_use(
            "wf-alias",
            None,
            None,
            None,
            None,
            Some(progress(
                1,
                serde_json::json!({ "type": "phase", "title": "scan", "state": "start" }),
            )),
        );
        o.update_streaming_tool_use(
            "wf-alias",
            None,
            None,
            None,
            None,
            Some(progress(
                2,
                serde_json::json!({ "type": "agent", "index": 1, "state": "start", "phaseTitle": "scan", "label": "alpha" }),
            )),
        );

        let tool = o.find_tool_use("wf-alias").unwrap();
        let progress = tool
            .raw_output
            .as_ref()
            .unwrap()
            .get("workflowProgress")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            progress.get("workflowName").and_then(Value::as_str),
            Some("aliased")
        );
        assert_eq!(
            progress
                .get("entries")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn workflow_final_raw_output_preserves_accumulated_progress() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("wf-1", "Workflow"));
        o.update_streaming_tool_use(
            "wf-1",
            None,
            None,
            None,
            None,
            Some(HashMap::from([
                ("type".into(), Value::String("workflow_progress".into())),
                ("runId".into(), Value::String("wf_test".into())),
                ("sequence".into(), Value::Number(1.into())),
                (
                    "entry".into(),
                    serde_json::json!({ "type": "log", "message": "boot" }),
                ),
            ])),
        );
        o.update_streaming_tool_use(
            "wf-1",
            Some(ToolCallStatus::Completed),
            None,
            None,
            None,
            Some(HashMap::from([
                ("status".into(), Value::String("completed".into())),
                ("runId".into(), Value::String("wf_test".into())),
                ("agentCount".into(), Value::Number(1.into())),
            ])),
        );

        let raw_output = o
            .find_tool_use("wf-1")
            .unwrap()
            .raw_output
            .as_ref()
            .unwrap();
        assert_eq!(
            raw_output.get("status").and_then(Value::as_str),
            Some("completed")
        );
        assert_eq!(
            raw_output
                .get("workflowProgress")
                .and_then(Value::as_object)
                .and_then(|progress| progress.get("entries"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn remove_streaming_tool_use_by_id() {
        let mut o = StreamingOverlay::new();
        o.upsert_streaming_tool_use(fake_tool_use("t1", "Read"));
        o.upsert_streaming_tool_use(fake_tool_use("t2", "Bash"));
        o.remove_streaming_tool_use("t1");
        assert_eq!(o.tool_use_count(), 1);
        assert!(o.find_tool_use("t2").is_some());
        o.remove_streaming_tool_use("missing");
        assert_eq!(o.tool_use_count(), 1);
    }

    #[test]
    fn clear_resets_all_fields() {
        let mut o = StreamingOverlay::new();
        o.set_streaming_text("partial");
        o.upsert_streaming_tool_use(fake_tool_use("t1", "Read"));
        o.append_streaming_thinking("reasoning...");
        o.clear();
        assert!(o.is_empty());
    }

    #[test]
    fn streaming_thinking_uses_thinking_field_not_text() {
        let t = StreamingThinking {
            thinking: "reasoning".into(),
            is_streaming: true,
            streaming_ended_at: Some(1_712_000_000_000),
        };
        assert_eq!(t.thinking, "reasoning");
    }

    #[test]
    fn streaming_text_has_visible_content_ignores_whitespace() {
        let mut o = StreamingOverlay::new();
        assert!(!o.streaming_text_has_visible_content());
        o.set_streaming_text("   \n\t ");
        assert!(!o.streaming_text_has_visible_content());
        o.set_streaming_text("");
        assert!(!o.streaming_text_has_visible_content());
        o.set_streaming_text("  real  ");
        assert!(o.streaming_text_has_visible_content());
    }

    #[test]
    fn combined_streaming_text_strips_leading_system_reminders() {
        let mut o = StreamingOverlay::new();
        o.set_streaming_text(
            "<system-reminder>one</system-reminder>\n<system-reminder>two</system-reminder>\nhello",
        );
        assert_eq!(o.combined_streaming_text().as_deref(), Some("hello"));
    }

    #[test]
    fn interleaved_blocks_preserve_order_through_full_lifecycle() {
        let mut o = StreamingOverlay::new();
        o.append_streaming_text("I'll read the file");
        o.upsert_streaming_tool_use(fake_tool_use("t1", "Read"));
        o.update_streaming_tool_use(
            "t1",
            Some(ToolCallStatus::Completed),
            None,
            None,
            None,
            None,
        );
        o.append_streaming_text("The file says hello");
        o.upsert_streaming_tool_use(fake_tool_use("t2", "Bash"));
        o.append_streaming_text("Done!");

        assert_eq!(o.blocks.len(), 5);
        assert!(
            matches!(&o.blocks[0], StreamingContentBlock::Text(t) if t == "I'll read the file")
        );
        assert!(
            matches!(&o.blocks[1], StreamingContentBlock::ToolUse(t) if t.call_id == "t1" && t.status == ToolCallStatus::Completed)
        );
        assert!(
            matches!(&o.blocks[2], StreamingContentBlock::Text(t) if t == "The file says hello")
        );
        assert!(matches!(&o.blocks[3], StreamingContentBlock::ToolUse(t) if t.call_id == "t2"));
        assert!(matches!(&o.blocks[4], StreamingContentBlock::Text(t) if t == "Done!"));
    }
}
