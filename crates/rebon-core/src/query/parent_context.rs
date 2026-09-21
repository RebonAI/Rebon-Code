use super::*;

pub(super) fn build_parent_context_capsule_from_messages(
    context: &rebon_tool::ContextRequest,
    messages: &[ApiMessage],
) -> Option<FrozenParentContextCapsule> {
    if matches!(context.mode, ContextShareMode::None) {
        return None;
    }

    let mut body = String::new();
    body.push_str("handoff_model: frozen_parent_context_capsule\n");
    body.push_str(&format!("context_mode: {}\n", context.mode.as_str()));
    body.push_str(&format!(
        "tool_results: {}\n",
        context.include_tool_results.as_str()
    ));
    body.push_str(&format!(
        "file_context: {}\n",
        context.include_files.as_str()
    ));
    if let Some(instructions) = context.instructions.as_deref() {
        body.push_str("instructions:\n");
        body.push_str(instructions.trim());
        body.push('\n');
    }
    body.push_str("policy: Observations below are parent-context facts only. Do not replay historical tool calls, tool results, or side effects. Inspect current files before editing.\n");

    if matches!(
        context.mode,
        ContextShareMode::CompactWithRecentTurns | ContextShareMode::TranscriptSlice
    ) {
        let requested_turns = context.include_recent_turns.unwrap_or(4).max(1);
        let recent = recent_textual_turns(messages, requested_turns, context.include_tool_results);
        if !recent.is_empty() {
            body.push_str("recent_turns:\n");
            for (idx, turn) in recent.iter().enumerate() {
                body.push_str(&format!("- turn_{}:\n", idx + 1));
                body.push_str(turn);
                if !turn.ends_with('\n') {
                    body.push('\n');
                }
            }
        }
    }

    if matches!(
        context.include_files,
        FileContextMode::References | FileContextMode::Contents
    ) {
        let files = referenced_file_paths(messages);
        if !files.is_empty() {
            body.push_str("referenced_files:\n");
            for file in files.into_iter().take(32) {
                body.push_str("- ");
                body.push_str(&file);
                body.push('\n');
            }
        }
    }

    let hash = rebon_api::stable_hash_str(&body);
    let text =
        format!("<ContextCapsule id=\"stable:{hash}\" version=\"1\">\n{body}</ContextCapsule>");
    Some(FrozenParentContextCapsule {
        token_estimate: estimate_text_tokens(&text),
        text: Arc::<str>::from(text),
        hash,
    })
}

pub(super) fn recent_textual_turns(
    messages: &[ApiMessage],
    max_turns: usize,
    tool_result_mode: ToolResultMode,
) -> Vec<String> {
    let mut turns = Vec::new();
    for message in messages.iter().rev() {
        if turns.len() >= max_turns {
            break;
        }
        if let Some(text) = summarize_message_for_capsule(message, tool_result_mode) {
            turns.push(text);
        }
    }
    turns.reverse();
    turns
}

pub(super) fn summarize_message_for_capsule(
    message: &ApiMessage,
    tool_result_mode: ToolResultMode,
) -> Option<String> {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    };
    let mut parts = Vec::new();
    for block in &message.content {
        match block {
            ApiContentBlock::Text(text) => {
                let trimmed = text.text.trim();
                if !trimmed.is_empty() {
                    parts.push(format!("text: {}", truncate_for_capsule(trimmed, 2000)));
                }
            }
            ApiContentBlock::ToolUse(tool_use) => {
                parts.push(format!(
                    "tool_use: name={} input_hash={}",
                    tool_use.name,
                    rebon_api::stable_hash_value(&tool_use.input)
                ));
            }
            ApiContentBlock::ToolResult(result) => {
                if matches!(tool_result_mode, ToolResultMode::None) {
                    continue;
                }
                let content_hash = rebon_api::stable_hash_str(&format!("{:?}", result.content));
                parts.push(format!(
                    "tool_result: tool_use_id={} is_error={} content_hash={}",
                    result.tool_use_id, result.is_error, content_hash
                ));
            }
            ApiContentBlock::Image(image) => {
                parts.push(format!(
                    "image: media_type={} data_hash={}",
                    image.source.media_type,
                    rebon_api::stable_hash_str(&image.source.data)
                ));
            }
            ApiContentBlock::Thinking(_) => {
                parts.push("thinking: omitted".to_string());
            }
            ApiContentBlock::ServerToolUse(tool) => {
                parts.push(format!("server_tool_use: name={}", tool.name));
            }
            ApiContentBlock::WebSearchResult(result) => {
                parts.push(format!(
                    "web_search_result: tool_use_id={}",
                    result.tool_use_id
                ));
            }
            ApiContentBlock::Compaction(block) => {
                if let Some(content) = block
                    .content
                    .as_deref()
                    .filter(|content| !content.is_empty())
                {
                    parts.push(format!(
                        "compaction: {}",
                        truncate_for_capsule(content, 2000)
                    ));
                }
            }
            ApiContentBlock::GeneratedImage(image) => {
                parts.push(format!(
                    "generated_image: id={} media_type={}",
                    image.id, image.media_type
                ));
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("  role: {role}\n  {}\n", parts.join("\n  ")))
    }
}

pub(super) fn referenced_file_paths(messages: &[ApiMessage]) -> Vec<String> {
    let mut files = std::collections::BTreeSet::new();
    for message in messages {
        for block in &message.content {
            match block {
                ApiContentBlock::Text(text) => collect_path_like_tokens(&text.text, &mut files),
                ApiContentBlock::ToolUse(tool_use) => {
                    collect_paths_from_value(&tool_use.input, &mut files)
                }
                ApiContentBlock::ToolResult(result) => {
                    collect_path_like_tokens(&format!("{:?}", result.content), &mut files);
                }
                _ => {}
            }
        }
    }
    files.into_iter().collect()
}

pub(super) fn collect_paths_from_value(
    value: &Value,
    out: &mut std::collections::BTreeSet<String>,
) {
    match value {
        Value::String(s) => collect_path_like_tokens(s, out),
        Value::Array(values) => {
            for value in values {
                collect_paths_from_value(value, out);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_paths_from_value(value, out);
            }
        }
        _ => {}
    }
}

pub(super) fn collect_path_like_tokens(text: &str, out: &mut std::collections::BTreeSet<String>) {
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                ',' | '.' | ':' | ';' | ')' | '(' | ']' | '[' | '"' | '\'' | '`'
            )
        });
        if token.len() < 3 || token.len() > 260 {
            continue;
        }
        let looks_like_path = token.contains('/')
            || token.contains('\\')
            || token.contains(".rs")
            || token.contains(".toml")
            || token.contains(".json")
            || token.contains(".md")
            || token.contains(".ts")
            || token.contains(".tsx")
            || token.contains(".js")
            || token.contains(".py");
        if looks_like_path {
            out.insert(token.replace('\\', "/"));
        }
    }
}

pub(super) fn truncate_for_capsule(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in value.chars().take(max_chars) {
        out.push(ch);
    }
    if value.chars().count() > max_chars {
        out.push_str("...<truncated>");
    }
    out
}

pub(super) fn estimate_text_tokens(value: &str) -> u32 {
    let chars = value.chars().count();
    chars.div_ceil(4).min(u32::MAX as usize) as u32
}
