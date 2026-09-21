use super::*;

#[allow(dead_code)]
pub(super) fn tool_status_label(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "running",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
    }
}

pub(super) fn hyperlink_path_label(path: &str, label: String, supports_hyperlinks: bool) -> String {
    if supports_hyperlinks {
        if let Some(url) = super::file_url::file_path_url(Path::new(path)) {
            return rebon_shell::create_hyperlink(&url, Some(&label), true);
        }
    }
    label
}

pub(super) fn location_label(location: &ToolCallLocation, supports_hyperlinks: bool) -> String {
    let label = match location.line {
        Some(line) => format!("{}:{line}", location.path),
        None => location.path.clone(),
    };
    hyperlink_path_label(&location.path, label, supports_hyperlinks)
}

pub(super) fn generated_image_saved_line(path: &str, supports_hyperlinks: bool) -> String {
    hyperlink_path_label(path, format!("Saved to: {path}"), supports_hyperlinks)
}

pub(super) fn is_image_generation_tool(name: &str) -> bool {
    matches!(name, "ImageGeneration" | "image_generation")
}

/// Strip OSC-8 wrappers before layout. Hyperlink metadata is attached to
/// rendered cells after the visible text has been painted.
pub(super) fn strip_osc8_sequences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek().copied() == Some(']') {
            chars.next();
            if chars.next() == Some('8') && chars.next() == Some(';') && chars.next() == Some(';') {
                for ch in chars.by_ref() {
                    if ch == '\x07' {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(ch);
    }
    out
}

pub(super) fn wrap_height_visible(text: &str, width: u16) -> u16 {
    wrap_height(&strip_osc8_sequences(text), width)
}

pub(super) fn string_display_width_without_osc8(text: &str) -> u16 {
    WidthStr::width(strip_osc8_sequences(text).as_str()) as u16
}

/// Attach hyperlink metadata to already-rendered cells. The Crossterm
/// backend emits OSC-8 separately from the visible symbol, so hyperlink
/// control bytes never participate in ratatui's width or diff calculations.
pub(super) fn apply_line_hyperlinks(lines: &[Line<'_>], area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut y = area.y;
    let mut x = area.x;
    let right = area.x.saturating_add(area.width);
    let bottom = area.y.saturating_add(area.height);

    for line in lines {
        for span in &line.spans {
            let content = span.content.as_ref();
            let mut rest = content;
            while let Some(start) = rest.find("\x1b]8;;") {
                let after_start = &rest[start + "\x1b]8;;".len()..];
                let Some(end_url) = after_start.find('\x07') else {
                    break;
                };
                let url = &after_start[..end_url];
                let after_url = &after_start[end_url + 1..];
                let Some(close_rel) = after_url.find("\x1b]8;;\x07") else {
                    break;
                };
                let display = &after_url[..close_rel];
                let before = &rest[..start];
                x = x.saturating_add(string_display_width_without_osc8(before));
                let start_x = x;
                let display_width = string_display_width_without_osc8(display);
                if display_width > 0 && y < bottom && start_x < right {
                    let end_x = start_x
                        .saturating_add(display_width.saturating_sub(1))
                        .min(right.saturating_sub(1));
                    for cell_x in start_x..=end_x {
                        if let Some(cell) = buf.cell_mut((cell_x, y)) {
                            cell.set_hyperlink(url);
                        }
                    }
                }
                x = x.saturating_add(display_width);
                rest = &after_url[close_rel + "\x1b]8;;\x07".len()..];
            }
            x = x.saturating_add(string_display_width_without_osc8(rest));
        }
        y = y.saturating_add(1);
        if y >= bottom {
            break;
        }
        x = area.x;
    }
}

pub(super) fn apply_markdown_hyperlink_layers(
    layers: &[rebon_message_tui::HyperlinkPaintLayer],
    buf: &mut Buffer,
) {
    const MARKER: Color = Color::Indexed(1);

    for layer in layers {
        if layer.area.width == 0 || layer.area.height == 0 {
            continue;
        }
        for hyperlink in &layer.hyperlinks {
            if !is_safe_terminal_hyperlink(&hyperlink.target) {
                continue;
            }
            let Some(mask_text) = hyperlink_mask_text(&layer.text, hyperlink, MARKER) else {
                continue;
            };
            let mask_area = Rect::new(0, 0, layer.area.width, layer.area.height);
            let mut mask = Buffer::empty(mask_area);
            Paragraph::new(mask_text)
                .wrap(Wrap { trim: false })
                .render(mask_area, &mut mask);

            for mask_y in 0..layer.area.height {
                let mut run_start = None;
                let mut marked_until = 0;
                for mask_x in 0..=layer.area.width {
                    let marked = if mask_x < layer.area.width {
                        let cell = &mask[(mask_x, mask_y)];
                        if cell.fg == MARKER {
                            let symbol_width = WidthStr::width(cell.symbol()).max(1) as u16;
                            marked_until = marked_until.max(mask_x.saturating_add(symbol_width));
                        }
                        mask_x < marked_until
                    } else {
                        false
                    };
                    match (run_start, marked) {
                        (None, true) => run_start = Some(mask_x),
                        (Some(start), false) => {
                            patch_hyperlink_cell_run(
                                buf,
                                layer.area.x.saturating_add(start),
                                layer.area.x.saturating_add(mask_x.saturating_sub(1)),
                                layer.area.y.saturating_add(mask_y),
                                &hyperlink.target,
                            );
                            run_start = None;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn hyperlink_mask_text(
    text: &ratatui::text::Text<'static>,
    hyperlink: &rebon_message_tui::HyperlinkRange,
    marker: Color,
) -> Option<ratatui::text::Text<'static>> {
    let line = text.lines.get(hyperlink.line)?;
    let visible = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    if hyperlink.start_byte >= hyperlink.end_byte
        || hyperlink.end_byte > visible.len()
        || !visible.is_char_boundary(hyperlink.start_byte)
        || !visible.is_char_boundary(hyperlink.end_byte)
    {
        return None;
    }

    let lines = text
        .lines
        .iter()
        .enumerate()
        .map(|(line_index, line)| {
            let content = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            if line_index != hyperlink.line {
                return Line::raw(content);
            }
            Line::from(vec![
                Span::raw(content[..hyperlink.start_byte].to_string()),
                Span::styled(
                    content[hyperlink.start_byte..hyperlink.end_byte].to_string(),
                    Style::new().fg(marker),
                ),
                Span::raw(content[hyperlink.end_byte..].to_string()),
            ])
        })
        .collect::<Vec<_>>();
    Some(ratatui::text::Text::from(lines))
}

fn patch_hyperlink_cell_run(buf: &mut Buffer, start_x: u16, end_x: u16, y: u16, target: &str) {
    if start_x > end_x || !buf.area().contains((start_x, y).into()) {
        return;
    }
    for x in start_x..=end_x {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_hyperlink(target);
        }
    }
}

fn is_safe_terminal_hyperlink(target: &str) -> bool {
    if target.is_empty()
        || target
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return false;
    }
    target
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        || target
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
}

pub(super) fn strip_osc8_from_lines(lines: &[Line<'static>]) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|line| {
            Line::from(
                line.spans
                    .iter()
                    .map(|span| {
                        Span::styled(strip_osc8_sequences(span.content.as_ref()), span.style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

// Compact JSON value formatting moved to the shared `rebon-render` so
// the GPUI app formats tool params identically. Re-exported so every
// `super::compact_json_value` / `super::is_default_json_value` call site (via
// `mod.rs use tool_helpers::*`) keeps resolving unchanged.
pub(super) use rebon_render::json_compact::{compact_json_value, is_default_json_value};

pub(super) fn is_agent_final_output(
    tool_name: &str,
    raw_output: Option<&HashMap<String, Value>>,
) -> bool {
    tool_name == "Agent"
        && raw_output
            .map(|output| output.contains_key("final_text") || output.contains_key("usage"))
            .unwrap_or(false)
}

pub(super) fn agent_map_string<'a>(
    map: Option<&'a HashMap<String, Value>>,
    keys: &[&str],
) -> Option<&'a str> {
    let map = map?;
    keys.iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
}

pub(super) fn is_plan_agent_final_output(
    tool: &crate::streaming::StreamingToolUse,
    raw_output: Option<&HashMap<String, Value>>,
) -> bool {
    if !is_agent_final_output(&tool.tool_name, raw_output) {
        return false;
    }

    [raw_output, tool.raw_input.as_ref()]
        .into_iter()
        .any(|map| {
            agent_map_string(
                map,
                &["agent_type", "agentType", "subagent_type", "subagentType"],
            )
            .is_some_and(|agent_type| agent_type.eq_ignore_ascii_case("Plan"))
        })
}

pub(super) fn raw_agent_read_file_count(raw_output: &HashMap<String, Value>) -> Option<u64> {
    raw_output
        .get("read_file_count")
        .or_else(|| raw_output.get("readFileCount"))
        .or_else(|| raw_output.get("read_files"))
        .and_then(Value::as_u64)
}

pub(super) fn agent_final_history_read_count(raw_output: &HashMap<String, Value>) -> Option<u64> {
    raw_output
        .get("sub_agent_tool_calls")
        .or_else(|| raw_output.get("subAgentToolCalls"))
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter(|call| {
                    let ok = call.get("ok").and_then(Value::as_bool).unwrap_or(true);
                    let is_read = call
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| matches!(name, "Read" | "FileReadTool"));
                    ok && is_read
                })
                .count() as u64
        })
}

fn agent_usage_token_counts(raw_output: &HashMap<String, Value>) -> Option<(u64, u64, u64)> {
    let usage = raw_output.get("usage").and_then(Value::as_object)?;
    let token_count = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| usage.get(*key).and_then(Value::as_u64))
    };
    let input_tokens = token_count(&["input_tokens", "inputTokens"]);
    let cache_read_tokens = token_count(&["cache_read_input_tokens", "cacheReadInputTokens"]);
    let prompt_cache_hit_tokens = token_count(&["prompt_cache_hit_tokens", "promptCacheHitTokens"]);
    let cumulative_output_tokens =
        token_count(&["cumulative_output_tokens", "cumulativeOutputTokens"]);
    let direct_output_tokens = token_count(&["output_tokens", "outputTokens"]);
    let output_tokens = cumulative_output_tokens
        .filter(|tokens| *tokens > 0)
        .or(direct_output_tokens)
        .or(cumulative_output_tokens);

    if input_tokens.is_none()
        && cache_read_tokens.is_none()
        && prompt_cache_hit_tokens.is_none()
        && output_tokens.is_none()
    {
        return None;
    }

    Some((
        input_tokens.unwrap_or(0),
        cache_read_tokens
            .unwrap_or(0)
            .max(prompt_cache_hit_tokens.unwrap_or(0)),
        output_tokens.unwrap_or(0),
    ))
}

pub(super) fn agent_usage_header_summary(raw_output: &HashMap<String, Value>) -> Option<String> {
    let (input_tokens, cached_tokens, output_tokens) = agent_usage_token_counts(raw_output)?;
    Some(format!(
        "Input {input_tokens} tokens, Cached {cached_tokens} tokens, Output {output_tokens} tokens"
    ))
}

pub(super) fn agent_final_summary_line(raw_output: &HashMap<String, Value>) -> String {
    let read_files = agent_final_history_read_count(raw_output)
        .or_else(|| raw_agent_read_file_count(raw_output))
        .unwrap_or(0);
    let (input_tokens, cached_tokens, output_tokens) =
        agent_usage_token_counts(raw_output).unwrap_or_default();
    format!(
        "Read {read_files} files, Input {input_tokens} tokens, Cached {cached_tokens} tokens, Output {output_tokens} tokens"
    )
}

pub(super) fn agent_final_text_lines(raw_output: &HashMap<String, Value>) -> Vec<String> {
    raw_output
        .get("final_text")
        .and_then(Value::as_str)
        .map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|lines| lines.iter().any(|line| !line.trim().is_empty()))
        .unwrap_or_else(|| vec![agent_final_summary_line(raw_output)])
}
