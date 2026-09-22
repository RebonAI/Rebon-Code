use std::collections::HashMap;

use rebon_types::{ContentBlock, ToolCallContent, ToolCallStatus};
use serde_json::Value;

use rebon_render::shell_output::shell_management_body_lines;

use super::{
    async_agent_launch_display, compact_json_map, compact_json_value, extract_agent_activity_lines,
    format_agent_token_count, is_image_generation_tool, is_read_image_raw_output, location_label,
    normalize_tool_body_lines, TranscriptRenderExtras,
};

// `render_tool_call_content` (the per-ToolCallContent-variant switch) moved to
// the shared `rebon-render::content` so the GPUI app renders Diff /
// Terminal / Text / Image / Resource identically. Re-exported so the
// `super::render_tool_call_content` call sites resolve unchanged.
pub(super) use rebon_render::content::render_tool_call_content;

// The agent_* body cluster moved to the shared rebon-render::agent
// so the GPUI app renders the Agent card body identically. Re-exported so the
// collect_tool_body_lines / tool_cards / message_render / transcript_segments
// call sites resolve unchanged.
pub(super) use rebon_render::agent::{agent_description, agent_full_detail_lines};

/// Flatten every tool-call body source (content / locations / raw_output)
/// into logical lines. Returned lines preserve their original order,
/// with multi-line content items split on `\n` so preview truncation
/// operates on lines the user would recognise from the underlying
/// tool output.
///
/// Empty trailing lines are kept because they carry meaning (e.g. a
/// file that ends with a blank line), but fully-empty sources (no
/// content, no locations, empty raw_output map) are skipped so
/// callers can treat `is_empty()` as "nothing worth rendering".
pub(super) fn collect_tool_body_lines(
    tool: &crate::streaming::StreamingToolUse,
    content_override: Option<&[ToolCallContent]>,
    supports_hyperlinks: bool,
    extras: TranscriptRenderExtras<'_>,
) -> Vec<String> {
    if let Some(text) = async_agent_launch_display(tool, extras) {
        return vec![text];
    }
    if let Some(lines) = workflow_journal_lines_for_tool(tool) {
        return lines;
    }
    if crate::streaming::is_workflow_tool_use(&tool.tool_name, tool.raw_input.as_ref()) {
        return Vec::new();
    }
    if tool.tool_name == "Sleep" {
        return Vec::new();
    }
    if is_web_search_tool(&tool.tool_name, tool.raw_input.as_ref()) {
        return tool
            .raw_output
            .as_ref()
            .and_then(|output| output.get("answer"))
            .and_then(Value::as_str)
            .map(|answer| answer.lines().map(str::to_string).collect())
            .unwrap_or_default();
    }
    if is_web_fetch_tool(&tool.tool_name, tool.raw_input.as_ref()) {
        // Errors carry no `content`; fall through so the generic
        // raw-output path still surfaces them.
        if let Some(content) = tool
            .raw_output
            .as_ref()
            .and_then(|output| output.get("content"))
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
        {
            return content.lines().map(str::to_string).collect();
        }
    }
    let content = content_override.or(tool.content.as_deref());
    if let Some(lines) = tool
        .raw_output
        .as_ref()
        .and_then(|raw_output| shell_management_body_lines(&tool.tool_name, raw_output))
    {
        return normalize_tool_body_lines(lines, true);
    }
    if let Some(lines) = collect_shell_tool_body_lines(
        &tool.tool_name,
        content,
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get("stdout"))
            .and_then(Value::as_str),
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get("stderr"))
            .and_then(Value::as_str),
        tool.raw_output
            .as_ref()
            .and_then(|output| output.get(rebon_tools_core::shell_stream_order::STREAM_ORDER_KEY))
            .and_then(Value::as_str),
        tool.status == ToolCallStatus::Failed,
    ) {
        return lines;
    }
    let is_agent_tool = tool.tool_name == "Agent";
    let mut lines: Vec<String> = Vec::new();
    if let Some(content) = content {
        for item in content {
            let s = render_tool_call_content(item);
            for l in s.split('\n') {
                if is_agent_tool {
                    append_unique_line(&mut lines, l);
                } else {
                    lines.push(l.to_string());
                }
            }
        }
    }
    if let Some(locations) = &tool.locations {
        let raw_output_is_read_image = tool
            .raw_output
            .as_ref()
            .map(|raw_output| is_read_image_raw_output(&tool.tool_name, raw_output))
            .unwrap_or(false);
        if !raw_output_is_read_image {
            for location in locations {
                let s = location_label(location, supports_hyperlinks);
                lines.push(s);
            }
        }
    }
    if let Some(raw_output) = &tool.raw_output {
        let output = if is_read_image_raw_output(&tool.tool_name, raw_output) {
            String::new()
        } else if is_agent_tool {
            if let Some(activity_lines) = extract_agent_activity_lines(raw_output) {
                for line in activity_lines {
                    append_unique_line(&mut lines, &line);
                }
                String::new()
            } else {
                compact_json_map(raw_output)
            }
        } else if is_image_generation_tool(&tool.tool_name) {
            let filtered: HashMap<String, Value> = raw_output
                .iter()
                .filter(|(key, _)| key.as_str() != "status")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            compact_json_map(&filtered)
        } else {
            compact_json_map(raw_output)
        };
        if !output.is_empty() {
            lines.push(output);
        }
    }
    normalize_tool_body_lines(lines, true)
}

pub(super) fn is_web_search_tool(
    tool_name: &str,
    raw_input: Option<&HashMap<String, Value>>,
) -> bool {
    tool_name == "WebSearch"
        || (tool_name == "InvokeDeferredTool"
            && raw_input
                .and_then(|input| input.get("tool_name"))
                .and_then(Value::as_str)
                == Some("WebSearch"))
}

pub(super) fn is_web_fetch_tool(
    tool_name: &str,
    raw_input: Option<&HashMap<String, Value>>,
) -> bool {
    tool_name == "WebFetch"
        || (tool_name == "InvokeDeferredTool"
            && raw_input
                .and_then(|input| input.get("tool_name"))
                .and_then(Value::as_str)
                == Some("WebFetch"))
}

// `interleaved_shell_streams` lives in the shared
// `rebon-render::shell_output` so the GPUI app replays the same
// stdout/stderr order from the same sketch.
use rebon_render::shell_output::interleaved_shell_streams;

pub(super) fn collect_shell_tool_body_lines(
    tool_name: &str,
    content: Option<&[ToolCallContent]>,
    stdout: Option<&str>,
    stderr: Option<&str>,
    stream_order: Option<&str>,
    include_content_with_raw_streams: bool,
) -> Option<Vec<String>> {
    if !rebon_render::streaming::is_live_shell_tool(tool_name) {
        return None;
    }

    let mut blocks: Vec<String> = Vec::new();
    // Completed results use the truncated raw streams; accumulated progress
    // content can contain the same output in its original unbounded form.
    if include_content_with_raw_streams || (stdout.is_none() && stderr.is_none()) {
        if let Some(content) = content {
            for item in content {
                let rendered = render_tool_call_content(item);
                let trimmed = rendered.trim();
                if !trimmed.is_empty() {
                    // Repeated progress blocks are meaningful shell output and must
                    // remain visible in explicit Verbose mode.
                    blocks.push(trimmed.to_string());
                }
            }
        }
    }
    let live_block_count = blocks.len();
    let mut live_output = None;
    // With a usable sketch the two streams become ONE block in arrival
    // order; without one they stay two blocks, stdout first — the shape
    // this function has always produced. Either way the dedupe against
    // the live progress blocks below is unchanged.
    let interleaved = interleaved_shell_streams(stdout, stderr, stream_order);
    let outputs: Vec<&str> = match interleaved.as_deref() {
        Some(merged) => vec![merged],
        None => [stdout, stderr].into_iter().flatten().collect(),
    };
    for output in outputs {
        let rendered = rebon_render::expand_tabs_for_tui(output);
        let trimmed = rendered.trim();
        if trimmed.is_empty() {
            continue;
        }
        if live_block_count > 0 {
            let existing = live_output.get_or_insert_with(|| blocks[..live_block_count].join("\n"));
            if output_block_contains(existing, trimmed) {
                continue;
            }
        }
        blocks.push(trimmed.to_string());
    }

    let lines = blocks
        .iter()
        .flat_map(|block| block.split('\n'))
        .map(|line| line.trim_end().to_string())
        .collect();
    Some(normalize_tool_body_lines(lines, true))
}

fn workflow_journal_lines_for_tool(
    tool: &crate::streaming::StreamingToolUse,
) -> Option<Vec<String>> {
    if tool.tool_name != "Read" {
        return None;
    }
    let file_path = tool
        .raw_output
        .as_ref()
        .and_then(|output| output.get("file"))
        .and_then(Value::as_object)
        .and_then(|file| file.get("filePath"))
        .and_then(Value::as_str)
        .or_else(|| {
            tool.raw_input
                .as_ref()
                .and_then(|input| input.get("file_path"))
                .and_then(Value::as_str)
        })?;
    if !file_path.ends_with("journal.jsonl") {
        return None;
    }
    let content = tool
        .raw_output
        .as_ref()
        .and_then(|output| output.get("file"))
        .and_then(Value::as_object)
        .and_then(|file| file.get("content"))
        .and_then(Value::as_str)
        .or_else(|| {
            tool.content.as_ref().and_then(|content| {
                content.iter().find_map(|item| match item {
                    ToolCallContent::Content(regular) => match &regular.content {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
            })
        })?;
    workflow_journal_lines(content)
}

fn workflow_journal_lines(content: &str) -> Option<Vec<String>> {
    let mut steps: Vec<WorkflowJournalStep> = Vec::new();
    let mut parsed_any = false;
    let mut ignored_non_empty = false;

    for (idx, line) in content.lines().enumerate() {
        let Some(json_text) = workflow_journal_json_text(line) else {
            if !line.trim().is_empty() {
                ignored_non_empty = true;
            }
            continue;
        };
        match serde_json::from_str::<Value>(json_text) {
            Ok(value) => {
                parsed_any = true;
                if let Some(event) = workflow_journal_event(&value, idx) {
                    apply_workflow_journal_event(&mut steps, event);
                }
            }
            Err(_) => ignored_non_empty = true,
        }
    }

    if !parsed_any || ignored_non_empty || steps.is_empty() {
        return None;
    }

    let mut lines = vec![format!("Workflow journal ({} steps)", steps.len())];
    lines.extend(steps.into_iter().map(|step| step.render()));
    Some(lines)
}

#[derive(Debug, Clone)]
struct WorkflowJournalStep {
    key: String,
    label: String,
    phase: Option<String>,
    state: WorkflowJournalStepState,
    tokens: u64,
    tool_calls: u64,
    duration_ms: Option<u64>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowJournalStepState {
    Started,
    Completed,
    Error,
}

impl WorkflowJournalStep {
    fn render(self) -> String {
        let prefix = self
            .phase
            .filter(|phase| !phase.trim().is_empty())
            .map(|phase| format!("[{phase}] "))
            .unwrap_or_default();
        match self.state {
            WorkflowJournalStepState::Started => format!("• {prefix}{} — started", self.label),
            WorkflowJournalStepState::Completed => {
                let metrics = workflow_step_metrics(self.tokens, self.tool_calls, self.duration_ms);
                if metrics.is_empty() {
                    format!("✓ {prefix}{} — completed", self.label)
                } else {
                    format!("✓ {prefix}{} — completed ({metrics})", self.label)
                }
            }
            WorkflowJournalStepState::Error => {
                let error = self.error.filter(|error| !error.trim().is_empty());
                if let Some(error) = error {
                    format!("✗ {prefix}{} — failed: {error}", self.label)
                } else {
                    format!("✗ {prefix}{} — failed", self.label)
                }
            }
        }
    }
}

struct WorkflowJournalEvent {
    key: String,
    label: String,
    phase: Option<String>,
    state: WorkflowJournalStepState,
    tokens: u64,
    tool_calls: u64,
    duration_ms: Option<u64>,
    error: Option<String>,
}

fn apply_workflow_journal_event(steps: &mut Vec<WorkflowJournalStep>, event: WorkflowJournalEvent) {
    if let Some(step) = steps.iter_mut().find(|step| step.key == event.key) {
        step.label = event.label;
        step.phase = event.phase.or_else(|| step.phase.take());
        step.state = event.state;
        step.tokens = event.tokens;
        step.tool_calls = event.tool_calls;
        step.duration_ms = event.duration_ms;
        step.error = event.error;
    } else {
        steps.push(WorkflowJournalStep {
            key: event.key,
            label: event.label,
            phase: event.phase,
            state: event.state,
            tokens: event.tokens,
            tool_calls: event.tool_calls,
            duration_ms: event.duration_ms,
            error: event.error,
        });
    }
}

fn workflow_journal_json_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('{') {
        return Some(trimmed);
    }
    let (_, rest) = trimmed.split_once('\t')?;
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

fn workflow_journal_event(value: &Value, index: usize) -> Option<WorkflowJournalEvent> {
    let object = value.as_object()?;
    let event_type = object.get("type").and_then(Value::as_str)?;
    let key = object
        .get("key")
        .map(compact_json_value)
        .unwrap_or_else(|| format!("event-{index}"));
    let opts = object.get("opts").and_then(Value::as_object);
    let label = opts
        .and_then(|opts| opts.get("label"))
        .and_then(Value::as_str)
        .or_else(|| object.get("prompt").and_then(Value::as_str))
        .map(first_non_empty_line)
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| key.clone());
    let phase = opts
        .and_then(|opts| opts.get("phase"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|phase| !phase.is_empty())
        .map(str::to_string);

    match event_type {
        "started" => Some(WorkflowJournalEvent {
            key,
            label,
            phase,
            state: WorkflowJournalStepState::Started,
            tokens: 0,
            tool_calls: 0,
            duration_ms: None,
            error: None,
        }),
        "result" => Some(WorkflowJournalEvent {
            key,
            label,
            phase,
            state: WorkflowJournalStepState::Completed,
            tokens: object.get("tokens").and_then(Value::as_u64).unwrap_or(0),
            tool_calls: object
                .get("toolCalls")
                .or_else(|| object.get("tool_calls"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            duration_ms: object
                .get("durationMs")
                .or_else(|| object.get("duration_ms"))
                .and_then(Value::as_u64),
            error: None,
        }),
        "error" => Some(WorkflowJournalEvent {
            key,
            label,
            phase,
            state: WorkflowJournalStepState::Error,
            tokens: 0,
            tool_calls: 0,
            duration_ms: None,
            error: object
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        _ => None,
    }
}

fn first_non_empty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(text.trim())
        .to_string()
}

fn workflow_step_metrics(tokens: u64, tool_calls: u64, duration: Option<u64>) -> String {
    let mut parts = Vec::new();
    if tokens > 0 {
        parts.push(format_agent_token_count(tokens));
    }
    if tool_calls > 0 {
        parts.push(format!(
            "{tool_calls} {}",
            if tool_calls == 1 { "tool" } else { "tools" }
        ));
    }
    if let Some(duration) = duration.filter(|duration| *duration > 0) {
        parts.push(format_duration_ms(duration));
    }
    parts.join(" · ")
}

fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms >= 1000 {
        let whole = duration_ms / 1000;
        let tenths = (duration_ms % 1000) / 100;
        if whole >= 10 {
            format!("{whole}s")
        } else if tenths == 0 {
            format!("{whole}s")
        } else {
            format!("{whole}.{tenths}s")
        }
    } else {
        format!("{duration_ms}ms")
    }
}

fn output_block_contains(existing: &str, candidate: &str) -> bool {
    let existing_lines = existing
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    let candidate_lines = candidate
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    existing_lines
        .windows(candidate_lines.len())
        .any(|window| window == candidate_lines)
}

fn append_unique_line(lines: &mut Vec<String>, line: &str) {
    let line = line.trim();
    if line.is_empty() || lines.iter().any(|existing| existing == line) {
        return;
    }
    lines.push(line.to_string());
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ratatui::{buffer::Buffer, layout::Rect};
    use rebon_types::{ContentBlock, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolKind};
    use serde_json::json;

    use super::super::{render_streaming_overlay, RenderTheme, ToolOutputVerbosity};
    use super::workflow_journal_lines;
    use crate::streaming::{StreamingOverlay, StreamingToolUse};

    fn new_buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area().width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.trim_end().to_string()
    }

    #[test]
    fn shell_output_keeps_repeated_lines_and_blank_separators() {
        let stdout = "step ok\n\nstep ok\ntail";
        let lines =
            super::collect_shell_tool_body_lines("Bash", None, Some(stdout), None, None, false)
                .expect("Bash is a shell tool");
        assert_eq!(lines, vec!["step ok", "", "step ok", "tail"]);
    }

    #[test]
    fn shell_output_dedupes_streams_embedded_in_content() {
        let content = vec![ToolCallContent::Content(rebon_types::RegularContent {
            content: ContentBlock::Text(rebon_types::TextContent {
                text: "out line\nerr line".into(),
                annotations: None,
            }),
        })];
        let lines = super::collect_shell_tool_body_lines(
            "PowerShell",
            Some(&content),
            Some("out line\nerr line"),
            Some("err line"),
            None,
            true,
        )
        .expect("PowerShell is a shell tool");
        assert_eq!(lines, vec!["out line", "err line"]);
    }

    #[test]
    fn live_shell_collection_handles_many_progress_chunks_without_quadratic_deduplication() {
        let content = (0..10_000)
            .map(|index| {
                ToolCallContent::Content(rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: format!("line-{index:05}"),
                        annotations: None,
                    }),
                })
            })
            .collect::<Vec<_>>();

        let lines = super::collect_shell_tool_body_lines(
            "PowerShell",
            Some(&content),
            None,
            None,
            None,
            false,
        )
        .expect("PowerShell is a shell tool");

        assert_eq!(lines.len(), 10_000);
        assert_eq!(lines.first().map(String::as_str), Some("line-00000"));
        assert_eq!(lines.last().map(String::as_str), Some("line-09999"));
    }

    #[test]
    fn live_shell_collection_keeps_identical_progress_chunks() {
        let repeated = || {
            ToolCallContent::Content(rebon_types::RegularContent {
                content: ContentBlock::Text(rebon_types::TextContent {
                    text: "same live line".into(),
                    annotations: None,
                }),
            })
        };
        let content = vec![repeated(), repeated()];

        let lines = super::collect_shell_tool_body_lines(
            "PowerShell",
            Some(&content),
            None,
            None,
            None,
            false,
        )
        .expect("PowerShell is a shell tool");

        assert_eq!(lines, vec!["same live line", "same live line"]);
    }

    #[test]
    fn shell_output_expands_tabs_in_raw_streams() {
        for tool_name in ["Bash", "PowerShell"] {
            for (stdout, stderr) in [
                (Some("209703\tcrates/rebon-cli/src/tui/agent_view.rs"), None),
                (None, Some("209703\tcrates/rebon-cli/src/tui/agent_view.rs")),
            ] {
                let lines = super::collect_shell_tool_body_lines(
                    tool_name, None, stdout, stderr, None, false,
                )
                .expect("shell tool");
                assert_eq!(lines, ["209703  crates/rebon-cli/src/tui/agent_view.rs"]);
            }
        }
    }

    #[test]
    fn shell_output_expands_tabs_after_interleaving_streams() {
        let lines = super::collect_shell_tool_body_lines(
            "Bash",
            None,
            Some("1\ta.rs\n2\tb.rs"),
            Some("error\tpath"),
            Some("o1,e1,o1"),
            false,
        )
        .expect("shell tool");
        assert_eq!(lines, ["1  a.rs", "error  path", "2  b.rs"]);
    }

    #[test]
    fn shell_output_deduplicates_tabbed_raw_streams_against_content() {
        let content = vec![ToolCallContent::Content(rebon_types::RegularContent {
            content: ContentBlock::Text(rebon_types::TextContent {
                text: "209703\tcrates/rebon-cli/src/tui/agent_view.rs".into(),
                annotations: None,
            }),
        })];
        for include_content in [false, true] {
            let lines = super::collect_shell_tool_body_lines(
                "Bash",
                Some(&content),
                Some("209703\tcrates/rebon-cli/src/tui/agent_view.rs"),
                None,
                None,
                include_content,
            )
            .expect("shell tool");
            assert_eq!(lines, ["209703  crates/rebon-cli/src/tui/agent_view.rs"]);
        }
    }

    #[test]
    fn shell_output_prefers_final_streams_over_live_progress_content() {
        let content = vec![ToolCallContent::Content(rebon_types::RegularContent {
            content: ContentBlock::Text(rebon_types::TextContent {
                text: "full live output that exceeded the final budget".into(),
                annotations: None,
            }),
        })];
        let final_stdout = "Warning: truncated output\n\nhead…truncated…tail";
        let lines = super::collect_shell_tool_body_lines(
            "Bash",
            Some(&content),
            Some(final_stdout),
            Some(""),
            None,
            false,
        )
        .expect("Bash is a shell tool");

        assert_eq!(
            lines,
            vec!["Warning: truncated output", "", "head…truncated…tail"]
        );
    }

    #[test]
    fn shell_output_keeps_identical_final_streams_when_not_in_live_content() {
        let lines = super::collect_shell_tool_body_lines(
            "PowerShell",
            None,
            Some("same final line"),
            Some("same final line"),
            None,
            false,
        )
        .expect("PowerShell is a shell tool");

        assert_eq!(lines, vec!["same final line", "same final line"]);
    }

    #[test]
    fn shell_output_keeps_distinct_overlapping_streams() {
        let lines = super::collect_shell_tool_body_lines(
            "Bash",
            None,
            Some("ok"),
            Some("not ok"),
            None,
            false,
        )
        .expect("Bash is a shell tool");
        assert_eq!(lines, vec!["ok", "not ok"]);
    }

    /// cargo writes every progress line (`Compiling`, `Finished`) to
    /// stderr and only test results to stdout. Concatenating the streams
    /// therefore renders the compile banner AFTER the test summary — the
    /// `streamOrder` sketch is what puts them back.
    #[test]
    fn shell_output_restores_arrival_order_from_the_stream_order_sketch() {
        let stdout = ["running 1 test", "test result: ok."].join("\n");
        let stderr = ["Compiling rebon-tool v0.20.0", "Finished test profile"].join("\n");
        let lines = super::collect_shell_tool_body_lines(
            "Bash",
            None,
            Some(&stdout),
            Some(&stderr),
            Some("e2,o2"),
            false,
        )
        .expect("Bash is a shell tool");
        assert_eq!(
            lines,
            vec![
                "Compiling rebon-tool v0.20.0",
                "Finished test profile",
                "running 1 test",
                "test result: ok.",
            ]
        );
    }

    #[test]
    fn shell_output_replays_a_genuinely_interleaved_run() {
        let stdout = ["a", "b", "c"].join("\n");
        let stderr = ["X", "Y"].join("\n");
        let lines = super::collect_shell_tool_body_lines(
            "PowerShell",
            None,
            Some(&stdout),
            Some(&stderr),
            Some("o1,e1,o1,e1,o1"),
            false,
        )
        .expect("PowerShell is a shell tool");
        assert_eq!(lines, vec!["a", "X", "b", "Y", "c"]);
    }

    /// Sessions recorded before `streamOrder` existed carry no sketch and
    /// must keep rendering exactly as they always did: stdout, then stderr.
    #[test]
    fn shell_output_without_a_sketch_keeps_stdout_before_stderr() {
        let stdout = ["running 1 test", "test result: ok."].join("\n");
        let stderr = ["Compiling rebon-tool v0.20.0"].join("\n");
        let lines = super::collect_shell_tool_body_lines(
            "Bash",
            None,
            Some(&stdout),
            Some(&stderr),
            None,
            false,
        )
        .expect("Bash is a shell tool");
        assert_eq!(
            lines,
            vec![
                "running 1 test",
                "test result: ok.",
                "Compiling rebon-tool v0.20.0",
            ]
        );
    }

    /// A sketch that no longer accounts for the lines it is handed — a
    /// truncated stream, a corrupted field — must fall back rather than
    /// zip the survivors into an order that never happened.
    #[test]
    fn shell_output_falls_back_when_the_sketch_does_not_match_the_streams() {
        let stdout = ["a", "b"].join("\n");
        let stderr = "X";
        for sketch in ["e1,o5", "o1,o1", "garbage", ""] {
            let lines = super::collect_shell_tool_body_lines(
                "Bash",
                None,
                Some(&stdout),
                Some(stderr),
                Some(sketch),
                false,
            )
            .expect("Bash is a shell tool");
            assert_eq!(lines, vec!["a", "b", "X"], "sketch {sketch:?}");
        }
    }

    /// An empty stream joins to `""`, which naive splitting would count
    /// as one blank line and knock the sketch's totals out of alignment.
    #[test]
    fn shell_output_treats_an_empty_stream_as_zero_lines() {
        let lines = super::collect_shell_tool_body_lines(
            "Bash",
            None,
            Some("only out"),
            Some(""),
            Some("e1,o1"),
            false,
        )
        .expect("Bash is a shell tool");
        assert_eq!(lines, vec!["only out"]);
    }

    #[test]
    fn compact_mode_streaming_overlay_shows_preview_with_hint() {
        // Body has three sources (content / locations / raw_output)
        // flattened into three preview lines — under the 4-line cap,
        // so no expand hint should appear here.
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-1".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::Completed,
            title: Some("Read Cargo.toml".into()),
            content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: "preview line".into(),
                        annotations: None,
                    }),
                },
            )]),
            locations: Some(vec![ToolCallLocation {
                path: "Cargo.toml".into(),
                line: Some(1),
            }]),
            raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
            raw_output: Some(HashMap::from([("bytes".into(), json!(123))])),
        });
        let mut buf = new_buf(80, 8);
        let used = render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        // Header + body preview.
        assert!(used >= 2);
        let snap = (0..8)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(snap.contains("Read (Cargo.toml)"), "{snap:?}");
        assert!(snap.contains("preview line"), "{snap:?}");
        assert!(snap.contains("Cargo.toml:1"), "{snap:?}");
        assert!(snap.contains("bytes=123"), "{snap:?}");
        // Three body lines fits within the preview cap → no inline truncation hint.
        assert!(!snap.contains("(Ctrl+O to expand)"), "{snap:?}");
    }

    #[test]
    fn compact_mode_streaming_overlay_truncates_long_content_with_hint() {
        // 10 content lines > 4-line preview cap → the first 4 are
        // shown and a `… +N lines (ctrl+o to expand)` hint is emitted.
        let long_text = (1..=10)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-1".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::Completed,
            title: None,
            content: Some(vec![ToolCallContent::Content(
                rebon_types::RegularContent {
                    content: ContentBlock::Text(rebon_types::TextContent {
                        text: long_text,
                        annotations: None,
                    }),
                },
            )]),
            locations: None,
            raw_input: Some(HashMap::from([("file_path".into(), json!("Cargo.toml"))])),
            raw_output: None,
        });
        let mut buf = new_buf(80, 10);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 80, 10),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Compact,
        );
        let snap = (0..10)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        // Preview shows the first four logical lines.
        assert!(snap.contains("line-1"), "{snap:?}");
        assert!(snap.contains("line-4"), "{snap:?}");
        // Lines past the cap are folded away.
        assert!(!snap.contains("line-5"), "{snap:?}");
        assert!(!snap.contains("line-10"), "{snap:?}");
        // And the user is told how many extra lines exist + how to see them.
        assert!(snap.contains("+6 lines"), "{snap:?}");
        assert!(snap.contains("Ctrl+O to expand"), "{snap:?}");
    }

    #[test]
    fn read_workflow_journal_jsonl_renders_steps() {
        let content = [
            r#"     1	{"type":"started","key":"agent:a","prompt":"Design UI\nextra","opts":{"label":"UI design","phase":"design"},"timestamp":1}"#,
            r#"     2	{"type":"result","key":"agent:a","prompt":"Design UI\nextra","opts":{"label":"UI design","phase":"design"},"tokens":1234,"toolCalls":2,"durationMs":6500,"timestamp":2}"#,
            r#"     3	{"type":"started","key":"agent:b","prompt":"Review parser","opts":{"label":"Parser review","phase":"review"},"timestamp":3}"#,
        ]
        .join("\n");
        let mut overlay = StreamingOverlay::new();
        overlay.upsert_streaming_tool_use(StreamingToolUse {
            call_id: "tool-1".into(),
            tool_name: "Read".into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::Completed,
            title: None,
            content: None,
            locations: None,
            raw_input: Some(HashMap::from([(
                "file_path".into(),
                json!("C:/tmp/workflows/wf_1/journal.jsonl"),
            )])),
            raw_output: Some(HashMap::from([(
                "file".into(),
                json!({
                    "filePath": "C:/tmp/workflows/wf_1/journal.jsonl",
                    "content": content,
                    "numLines": 3,
                    "startLine": 1,
                    "totalLines": 3
                }),
            )])),
        });

        let mut buf = new_buf(120, 8);
        render_streaming_overlay(
            &overlay,
            Rect::new(0, 0, 120, 8),
            &mut buf,
            &RenderTheme::plain(),
            ToolOutputVerbosity::Verbose,
        );
        let snap = (0..8)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(snap.contains("Workflow journal (2 steps)"), "{snap:?}");
        assert!(
            snap.contains("✓ [design] UI design — completed (1.2k tokens · 2 tools · 6.5s)"),
            "{snap:?}"
        );
        assert!(
            snap.contains("• [review] Parser review — started"),
            "{snap:?}"
        );
        assert!(!snap.contains(r#"{"type":"started""#), "{snap:?}");
    }

    #[test]
    fn read_workflow_journal_plain_jsonl_renders_steps() {
        let content = [
            r#"{"type":"started","key":"agent:a","prompt":"Design UI","opts":{"phase":"design"},"timestamp":1}"#,
            r#"{"type":"result","key":"agent:a","prompt":"Design UI","opts":{"phase":"design"},"tokens":9,"toolCalls":0,"durationMs":80,"timestamp":2}"#,
        ]
        .join("\n");
        let lines = workflow_journal_lines(&content).expect("workflow journal should parse");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "Workflow journal (1 steps)");
        assert_eq!(
            lines[1],
            "✓ [design] Design UI — completed (9 tokens · 80ms)"
        );
    }
}
