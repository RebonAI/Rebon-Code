//! Which rows fold together, decided once.
//!
//! Every surface folding this transcript asks the same question here, so all
//! of them get the same answer instead of one set of rules per front-end. This
//! is that walk with nothing drawn.
//!
//! The plan is by index, not by owned rows: [`TranscriptSegment`] names
//! positions in the slice it was handed. That is what lets a caller key a
//! memoised segment by the rows behind it, and it leaves a caller that wants
//! owned rows free to build them.
//!
//! Drawing and measuring stay with the consumer, as do the live-agent activity
//! signatures a render cache invalidates on.

use serde_json::Value;

use crate::collapse_grouping::{classify_tool_use, ClassifyOptions, ToolClass};
use crate::transcript_row::{AssistantContentBlock, Message, UserContentBlock};
use crate::user_text_kind::{detect_user_text_kind, UserTextKind};
use crate::{MemoryPathPolicy, TRANSCRIPT_HIDDEN_TOOLS};

/// If `msg` is an assistant message that carries at least one `ToolUse`
/// block, return the tool_uses it contains. Otherwise `None`.
///
/// A grouping predicate that only accepted one-tool-per-message rows
/// would miss most turns: rebon bundles an entire turn into a single [`AssistantMessage`](crate::transcript_row::AssistantMessage): a brief text preamble
/// ("Let me search for X…"), an optional `Thinking` block (extended
/// thinking / reasoning models), and every `ToolUse` the model emitted
/// in that turn all live in the same `content` array. This is ALSO the
/// shape every persisted assistant entry takes: the full content array is
/// written unchanged, so transcript replay restores messages with
/// `[Thinking, ToolUse×N]`. Rejecting anything with a non-`ToolUse` block
/// would
/// mean the collapse almost never fires in practice, because models
/// routinely narrate AND reason before calling tools.
///
/// So `Thinking` / `RedactedThinking` blocks are unconditionally
/// tolerated, and whitespace-only `Text` blocks are tolerated as well.
/// Non-empty `Text` is NOT tolerated before or after any `ToolUse`:
/// narration and trailing answers are user-visible assistant output,
/// and absorbing them into a compact tool summary would make them
/// disappear. Such messages break the run and render through the normal
/// widget.
pub fn tool_only_assistant(
    msg: &Message,
) -> Option<Vec<&crate::transcript_row::AssistantToolUseBlock>> {
    if let Message::Assistant(a) = msg {
        let mut tools: Vec<&crate::transcript_row::AssistantToolUseBlock> = Vec::new();
        for block in &a.message.content {
            match block {
                AssistantContentBlock::ToolUse(t) => {
                    tools.push(t);
                }
                AssistantContentBlock::Text(text) => {
                    if !text.text.trim().is_empty() {
                        return None;
                    }
                }
                _ => {}
            }
        }
        if tools.is_empty() {
            None
        } else {
            Some(tools)
        }
    } else {
        None
    }
}

/// If `msg` is a user message whose `content` array contains only
/// `ToolResult` blocks (and at least one), return them. Otherwise `None`.
pub fn tool_result_only_user(
    msg: &Message,
) -> Option<Vec<&crate::transcript_row::UserToolResultBlock>> {
    if let Message::User(u) = msg {
        if u.is_meta == Some(true) {
            return None;
        }
        if u.message.content.is_empty() {
            return None;
        }
        let mut results = Vec::new();
        for block in &u.message.content {
            match block {
                UserContentBlock::ToolResult(r) => results.push(r),
                _ => return None,
            }
        }
        Some(results)
    } else {
        None
    }
}

/// `true` iff `msg` is a user message flagged `isMeta: true` — an
/// attachment/reminder injection the engine persists to the transcript
/// for replay fidelity but which renders as nothing (see `to_message_row`).
/// These rows must be *transparent* to the collapse
/// grouping: the engine inserts one between every iteration
/// (`query.rs` AttachmentInjected handler), so a naive walk that treats
/// them as run-breakers chops every consecutive read/search chain into
/// 1-tool fragments and the summary never fires.
pub fn is_meta_user(msg: &Message) -> bool {
    matches!(msg, Message::User(u) if u.is_meta == Some(true))
}

/// `true` iff `msg` is an assistant message whose only content is
/// `Thinking` / `RedactedThinking` blocks (no `Text`, no `ToolUse`).
/// Reasoning models emit `[thinking, tool_use]` per turn; when the
/// tool_use is a hidden-UI tool (TaskCreate, TaskUpdate, etc) it gets
/// filtered out at replay (`strip_hidden_tool_uses`) or at streaming
/// (`is_streaming_hidden_tool`), leaving a dangling thinking-only row.
///
/// These rows must be transparent to the collapse walk for the same
/// reason as `is_meta_user`: the engine interleaves them between every
/// otherwise-consecutive `[thinking, Read]` / `[thinking, Glob]` row,
/// so a naive walk treats them as run-breakers and every collapsible
/// chain fragments into 1-tool Singles.
pub fn is_thinking_only_assistant(msg: &Message) -> bool {
    match msg {
        Message::Assistant(a) => {
            let mut saw_thinking = false;
            for block in &a.message.content {
                match block {
                    AssistantContentBlock::Thinking(_)
                    | AssistantContentBlock::RedactedThinking(_) => {
                        saw_thinking = true;
                    }
                    AssistantContentBlock::Text(_)
                    | AssistantContentBlock::ToolUse(_)
                    | AssistantContentBlock::GeneratedImage(_) => {
                        return false;
                    }
                    AssistantContentBlock::Other => return false,
                }
            }
            saw_thinking
        }
        _ => false,
    }
}

/// Classify every tool_use block on an assistant message. Returns `None`
/// if any block fails `is_collapsible()` — the whole message is then
/// ineligible for the collapsed run.
pub fn classify_message_tool_uses(
    tools: &[&crate::transcript_row::AssistantToolUseBlock],
) -> Option<Vec<ToolClass>> {
    let policy = MemoryPathPolicy::default();
    let options = ClassifyOptions::default();
    let mut out = Vec::with_capacity(tools.len());
    for t in tools {
        let class = classify_tool_use(&t.name, &t.input, &policy, options);
        if !class.is_collapsible() {
            return None;
        }
        out.push(class);
    }
    Some(out)
}

pub fn trailing_collapsible_tool_run_start(
    rows: &[Message],
    render_thinking_only_rows: bool,
) -> Option<usize> {
    let mut pending_thinking_start = None;
    let mut run_start = None;
    let mut saw_tool = false;

    for (idx, row) in rows.iter().enumerate() {
        if is_meta_user(row) {
            continue;
        }
        if is_thinking_only_assistant(row) && !render_thinking_only_rows {
            if run_start.is_none() {
                pending_thinking_start.get_or_insert(idx);
            }
            continue;
        }
        if tool_result_only_user(row).is_some() && run_start.is_some() {
            continue;
        }
        if let Some(tools) = tool_only_assistant(row) {
            if classify_message_tool_uses(&tools).is_some() {
                run_start.get_or_insert_with(|| pending_thinking_start.take().unwrap_or(idx));
                saw_tool = true;
                continue;
            }
        }

        pending_thinking_start = None;
        run_start = None;
        saw_tool = false;
    }

    saw_tool.then(|| run_start.expect("collapsible tool run must have a start"))
}

/// Unit of render work built from the committed rows. Either a single
/// row (rendered with the normal `render_message_inner` widget) or a
/// maximal run of `>= 2` collapsible tool_uses + their tool_result
/// companions that render as one summary block.
#[derive(Debug, Clone)]
pub enum TranscriptSegment {
    Single(usize),
    Collapsed { indices: Vec<usize> },
    AgentGroup { indices: Vec<usize> },
    ThinkingGroup { segments: Vec<TranscriptSegment> },
}

impl TranscriptSegment {
    pub fn first(&self) -> usize {
        match self {
            Self::Single(i) => *i,
            Self::Collapsed { indices } | Self::AgentGroup { indices } => indices[0],
            Self::ThinkingGroup { segments } => segments[0].first(),
        }
    }

    pub fn contains(&self, row: usize) -> bool {
        match self {
            Self::Single(i) => *i == row,
            Self::Collapsed { indices } | Self::AgentGroup { indices } => indices.contains(&row),
            Self::ThinkingGroup { segments } => {
                segments.iter().any(|segment| segment.contains(row))
            }
        }
    }

    pub fn row_indices(&self) -> Vec<usize> {
        match self {
            Self::Single(i) => vec![*i],
            Self::Collapsed { indices } | Self::AgentGroup { indices } => indices.clone(),
            Self::ThinkingGroup { segments } => segments
                .iter()
                .flat_map(TranscriptSegment::row_indices)
                .collect(),
        }
    }
}

pub fn transcript_segment_contains_collapsed(segment: &TranscriptSegment) -> bool {
    match segment {
        TranscriptSegment::Collapsed { .. } => true,
        TranscriptSegment::ThinkingGroup { segments } => {
            segments.iter().any(transcript_segment_contains_collapsed)
        }
        TranscriptSegment::Single(_) | TranscriptSegment::AgentGroup { .. } => false,
    }
}

pub fn async_agent_launch_only_assistant_tools(
    msg: &Message,
) -> Option<Vec<&crate::transcript_row::AssistantToolUseBlock>> {
    let Message::Assistant(a) = msg else {
        return None;
    };
    let mut agent_tools = Vec::new();
    for block in &a.message.content {
        match block {
            AssistantContentBlock::ToolUse(tool) => {
                if !is_async_agent_launch_value(tool) {
                    return None;
                }
                agent_tools.push(tool);
            }
            AssistantContentBlock::Text(text) if text.text.trim().is_empty() => {}
            AssistantContentBlock::Thinking(_) | AssistantContentBlock::RedactedThinking(_) => {}
            AssistantContentBlock::Text(_)
            | AssistantContentBlock::GeneratedImage(_)
            | AssistantContentBlock::Other => return None,
        }
    }
    (!agent_tools.is_empty()).then_some(agent_tools)
}

pub fn is_async_agent_launch_value(tool: &crate::transcript_row::AssistantToolUseBlock) -> bool {
    if tool.name != "Agent" {
        return false;
    }
    tool.raw_output
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|output| output.get("status"))
        .and_then(Value::as_str)
        == Some("async_launched")
}

fn async_agent_launch_run(rows: &[Message], start: usize) -> Option<(Vec<usize>, usize)> {
    async_agent_launch_only_assistant_tools(rows.get(start)?)?;
    let mut indices = vec![start];
    let mut agent_count = async_agent_launch_only_assistant_tools(&rows[start])?.len();
    let mut j = start + 1;
    while j < rows.len() {
        if is_meta_user(&rows[j]) {
            j += 1;
            continue;
        }
        if let Some(tools) = async_agent_launch_only_assistant_tools(&rows[j]) {
            agent_count += tools.len();
            indices.push(j);
            j += 1;
            continue;
        }
        break;
    }
    (agent_count >= 2).then_some((indices, j))
}

/// Walk committed `rows` and fold runs of consecutive pure-tool-use
/// assistant + pure-tool-result user messages into `Collapsed` segments,
/// emitting one `Single` segment per other row.
///
/// A run only becomes `Collapsed` when the total number of `tool_use`
/// blocks across the run is `>= 2` — a lone tool call still renders as a
/// per-tool card.
fn build_base_transcript_segments(
    rows: &[Message],
    render_thinking_only_rows: bool,
) -> Vec<TranscriptSegment> {
    let mut segments: Vec<TranscriptSegment> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if is_meta_user(&rows[i]) {
            i += 1;
            continue;
        }

        let mut indices: Vec<usize> = Vec::new();
        let mut head = i;
        if is_thinking_only_assistant(&rows[i]) && !render_thinking_only_rows {
            indices.push(i);
            head += 1;
            while head < rows.len() {
                if is_meta_user(&rows[head]) {
                    head += 1;
                    continue;
                }
                if is_thinking_only_assistant(&rows[head]) {
                    indices.push(head);
                    head += 1;
                    continue;
                }
                break;
            }
        }

        let preserved_thinking_indices = indices.clone();

        if head >= rows.len() {
            i = head;
            continue;
        }

        if let Some((mut agent_indices, next)) = async_agent_launch_run(rows, head) {
            if !preserved_thinking_indices.is_empty() {
                let mut grouped_indices = preserved_thinking_indices;
                grouped_indices.append(&mut agent_indices);
                segments.push(TranscriptSegment::AgentGroup {
                    indices: grouped_indices,
                });
            } else {
                segments.push(TranscriptSegment::AgentGroup {
                    indices: agent_indices,
                });
            }
            i = next;
            continue;
        }

        let head_tools = match rows.get(head).and_then(tool_only_assistant) {
            Some(tools) => tools,
            None => {
                if message_allows_prior_thinking_preview(&rows[head]) {
                    for idx in preserved_thinking_indices {
                        segments.push(TranscriptSegment::Single(idx));
                    }
                }
                segments.push(TranscriptSegment::Single(head));
                i = head + 1;
                continue;
            }
        };
        if classify_message_tool_uses(&head_tools).is_none() {
            if message_allows_prior_thinking_preview(&rows[head]) {
                for idx in preserved_thinking_indices {
                    segments.push(TranscriptSegment::Single(idx));
                }
            }
            segments.push(TranscriptSegment::Single(head));
            i = head + 1;
            continue;
        };

        if indices.last().copied() != Some(head) {
            indices.push(head);
        }
        let mut tool_count: usize = head_tools.len();
        let mut j = head + 1;
        while j < rows.len() {
            if is_meta_user(&rows[j]) {
                j += 1;
                continue;
            }
            if is_thinking_only_assistant(&rows[j]) && !render_thinking_only_rows {
                indices.push(j);
                j += 1;
                continue;
            }
            if tool_result_only_user(&rows[j]).is_some() {
                indices.push(j);
                j += 1;
                continue;
            }
            if let Some(tools) = tool_only_assistant(&rows[j]) {
                if classify_message_tool_uses(&tools).is_some() {
                    tool_count += tools.len();
                    indices.push(j);
                    j += 1;
                    continue;
                }
            }
            break;
        }

        if tool_count >= 2 {
            segments.push(TranscriptSegment::Collapsed { indices });
        } else {
            for idx in indices {
                segments.push(TranscriptSegment::Single(idx));
            }
        }
        i = j;
    }
    segments
}

/// Decide which of `rows` fold together, and how.
///
/// The returned plan covers every row exactly once, in order, as indices
/// into the slice passed in. Pass `render_thinking_only_rows` when the
/// caller draws thinking-only assistant rows on their own; when it is
/// false those rows are transparent to the walk, which is what keeps a
/// `[thinking, Read]` chain from fragmenting into single rows.
pub fn fold_rows(rows: &[Message], render_thinking_only_rows: bool) -> Vec<TranscriptSegment> {
    group_thinking_segments(
        rows,
        build_base_transcript_segments(rows, render_thinking_only_rows),
    )
}

fn transcript_segment_thinking_count(rows: &[Message], segment: &TranscriptSegment) -> usize {
    match segment {
        TranscriptSegment::Single(idx) => message_thinking_count(&rows[*idx]),
        TranscriptSegment::Collapsed { indices } | TranscriptSegment::AgentGroup { indices } => {
            indices
                .iter()
                .map(|idx| message_thinking_count(&rows[*idx]))
                .sum()
        }
        TranscriptSegment::ThinkingGroup { segments } => segments
            .iter()
            .map(|segment| transcript_segment_thinking_count(rows, segment))
            .sum(),
    }
}

fn transcript_segment_breaks_thinking_group(rows: &[Message], segment: &TranscriptSegment) -> bool {
    let TranscriptSegment::Single(idx) = segment else {
        return false;
    };
    let row = &rows[*idx];
    if is_thinking_only_assistant(row) || is_meta_user(row) || tool_result_only_user(row).is_some()
    {
        return false;
    }
    if let Some(tools) = tool_only_assistant(row) {
        return tools.iter().any(|tool| {
            !TRANSCRIPT_HIDDEN_TOOLS
                .iter()
                .any(|hidden| *hidden == tool.name)
        });
    }
    true
}

fn group_thinking_segments(
    rows: &[Message],
    segments: Vec<TranscriptSegment>,
) -> Vec<TranscriptSegment> {
    let mut grouped = Vec::new();
    let mut pending = Vec::new();
    let mut thinking_count = 0usize;

    for segment in segments {
        if matches!(segment, TranscriptSegment::Collapsed { .. }) {
            flush_pending_thinking_group(&mut grouped, &mut pending, &mut thinking_count);
            grouped.push(segment);
            continue;
        }

        if transcript_segment_breaks_thinking_group(rows, &segment) {
            flush_pending_thinking_group(&mut grouped, &mut pending, &mut thinking_count);
            if transcript_segment_thinking_count(rows, &segment) >= 2 {
                grouped.push(TranscriptSegment::ThinkingGroup {
                    segments: vec![segment],
                });
            } else {
                grouped.push(segment);
            }
            continue;
        }

        let segment_thinking_count = transcript_segment_thinking_count(rows, &segment);
        if pending.is_empty() && segment_thinking_count == 0 {
            grouped.push(segment);
            continue;
        }

        thinking_count += segment_thinking_count;
        pending.push(segment);
    }

    flush_pending_thinking_group(&mut grouped, &mut pending, &mut thinking_count);
    grouped
}

fn flush_pending_thinking_group(
    grouped: &mut Vec<TranscriptSegment>,
    pending: &mut Vec<TranscriptSegment>,
    thinking_count: &mut usize,
) {
    if *thinking_count >= 2 {
        grouped.push(TranscriptSegment::ThinkingGroup {
            segments: std::mem::take(pending),
        });
    } else {
        grouped.append(pending);
    }
    *thinking_count = 0;
}

fn message_thinking_count(message: &Message) -> usize {
    match message {
        Message::Assistant(assistant) => assistant
            .message
            .content
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    AssistantContentBlock::Thinking(thinking)
                        if !thinking.thinking.trim().is_empty()
                )
            })
            .count(),
        _ => 0,
    }
}

/// Is this user text a coordinator's own traffic rather than something a
/// person typed? Teammate messages and task notifications ride in as user
/// rows, and the fold keeps them from breaking a thinking group the way a
/// real prompt does.
pub fn user_text_is_coordinator_notification(text: &str) -> bool {
    matches!(
        detect_user_text_kind(text),
        UserTextKind::TeammateMessage | UserTextKind::TaskNotification
    )
}

pub fn user_block_breaks_thinking_boundary(block: &UserContentBlock) -> bool {
    match block {
        UserContentBlock::ToolResult(_) => false,
        UserContentBlock::Text(text) => !user_text_is_coordinator_notification(&text.text),
        UserContentBlock::Image(_) => true,
    }
}

pub fn message_has_non_tool_user_content(msg: &Message) -> bool {
    matches!(msg, Message::User(u) if u.message.content.iter().any(user_block_breaks_thinking_boundary))
}

pub fn message_allows_prior_thinking_preview(msg: &Message) -> bool {
    match msg {
        Message::Assistant(a) => a.message.content.iter().any(|block| {
            matches!(
                block,
                AssistantContentBlock::Text(_)
                    | AssistantContentBlock::GeneratedImage(_)
                    | AssistantContentBlock::Other
            )
        }),
        Message::User(u) => u.message.content.iter().any(|block| {
            matches!(
                block,
                UserContentBlock::Text(text) if user_text_is_coordinator_notification(&text.text)
            )
        }),
        _ => false,
    }
}
