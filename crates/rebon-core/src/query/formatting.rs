use super::*;

pub(super) fn channel_permission_broker_from(
    broker: &dyn PermissionBroker,
) -> Option<&crate::permission::ChannelPermissionBroker> {
    if let Some(cpb) = broker
        .as_any()
        .downcast_ref::<crate::permission::ChannelPermissionBroker>()
    {
        return Some(cpb);
    }
    if let Some(hooked_broker) = broker.as_any().downcast_ref::<HookedPermissionBroker>() {
        return channel_permission_broker_from(hooked_broker.inner().as_ref());
    }
    if let Some(rules_broker) = broker
        .as_any()
        .downcast_ref::<crate::policy::RulesBasedPermissionBroker>()
    {
        return channel_permission_broker_from(rules_broker.delegate().as_ref());
    }
    None
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        let boundary = floor_char_boundary(s, max.saturating_sub(3));
        format!("{}...", &s[..boundary])
    } else {
        s.to_string()
    }
}

/// Name of the tool whose result shape `value` actually has.
///
/// `InvokeDeferredTool` runs the target tool and returns its result
/// verbatim, so a gateway call reports the gateway's name over the
/// target's payload. Left unresolved, every per-tool branch below misses
/// and the result falls through to raw JSON — a 20 K-char WebFetch page
/// comes back with every newline and quote escaped, roughly doubling its
/// tokens, and its paging hint is lost. The gateway input names the
/// target; unwrap it once here so a tool formats the same however it was
/// reached. Only deferred tools are reachable this way (WebFetch and
/// Workflow are the ones with a branch of their own); Read, Skill, Edit
/// and Write are eager and always arrive under their own name.
pub(super) fn effective_tool_name<'a>(tool_name: &'a str, input: Option<&'a Value>) -> &'a str {
    if tool_name != rebon_tool::INVOKE_DEFERRED_TOOL_NAME {
        return tool_name;
    }
    input
        .and_then(|input| input.get("tool_name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(tool_name)
}

/// Convert a tool's raw JSON output into compact model-visible
/// `tool_result` content.
///
/// `input` is the tool call's arguments, used only to see through the
/// `InvokeDeferredTool` gateway; pass `None` where they aren't at hand.
///
/// `tools` is the turn's resolver, used only to ask the tool that produced
/// `value` whether it projects its own result
/// ([`rebon_tool::Tool::project_result_for_model`]). A tool whose result
/// needs a shape the engine cannot derive from JSON alone — an image block
/// beside a summary line — answers there instead of earning a branch here.
/// Pass `None` where no resolver is at hand; the generic projection below
/// then applies, as it does for every tool that declines.
///
/// Edit and Write send only a confirmation message — not the full file
/// content — back to the model, so they don't inflate the context window.  A 715 K-char
/// file edited five times would otherwise produce ~895 K tokens of
/// tool-result content in a single user message.
pub(super) fn compact_tool_result_for_model(
    tool_name: &str,
    input: Option<&Value>,
    value: &Value,
    tools: Option<&dyn rebon_tool::ToolResolver>,
) -> ToolResultContent {
    // Resolved under the *effective* name so a tool projects its own result
    // the same way whether the model called it directly or through the
    // `InvokeDeferredTool` gateway. No filter: the tool already ran, so the
    // question here is only which crate knows the shape of what came back.
    if let Some(resolver) = tools {
        let owner = match resolver.resolve(effective_tool_name(tool_name, input), None) {
            Ok(owner) => owner,
            Err(error) => {
                // Falling through to the generic projection is the safe answer
                // and the one this function gave before tools could project;
                // say so rather than losing the reason.
                tracing::debug!(
                    tool = tool_name,
                    %error,
                    "could not resolve the tool that produced this result; \
                     using the generic model projection"
                );
                None
            }
        };
        if let Some(content) = owner.and_then(|tool| tool.project_result_for_model(value)) {
            return content;
        }
    }
    match effective_tool_name(tool_name, input) {
        "AskUserQuestion" => {
            ToolResultContent::text(format_ask_user_question_result_for_model(value))
        }
        "Skill" => {
            if let Some(prompt) = value.get("prompt").and_then(Value::as_str) {
                ToolResultContent::text(truncate_tool_result_content(prompt.to_string()))
            } else {
                let raw = serde_json::to_string(value).unwrap_or_default();
                ToolResultContent::text(truncate_tool_result_content(raw))
            }
        }
        "Workflow" | "RunWorkflow" => {
            // `workflowProgress` is render-only telemetry for the TUI workflow
            // card (the phase/agent/log tree). The model only needs the
            // result, status, and run metadata, so strip it before
            // serializing to keep the tool result compact.
            let value = match value.as_object() {
                Some(object) if object.contains_key("workflowProgress") => {
                    let mut object = object.clone();
                    object.remove("workflowProgress");
                    Value::Object(object)
                }
                _ => value.clone(),
            };
            let raw = serde_json::to_string(&value).unwrap_or_default();
            ToolResultContent::text(truncate_tool_result_content(raw))
        }
        "Read" => read_tool_result_for_model(value, true),
        "WebFetch" => ToolResultContent::text(truncate_tool_result_content(
            format_web_fetch_result_for_model(value),
        )),
        "Edit" | "FileEditTool" => {
            let path = value
                .get("filePath")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let replacements = value
                .get("replacements")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            if replacements > 1 {
                ToolResultContent::text(format!(
                    "The file {path} has been updated. \
                     All {replacements} occurrences were successfully replaced."
                ))
            } else {
                ToolResultContent::text(format!("The file {path} has been updated successfully."))
            }
        }
        "Write" | "FileWriteTool" => {
            let path = value
                .get("filePath")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let num_lines = value.get("numLines").and_then(Value::as_u64);
            let write_type = value.get("type").and_then(Value::as_str).unwrap_or("write");
            match (write_type, num_lines) {
                ("create", Some(n)) => ToolResultContent::text(format!(
                    "The file {path} has been created successfully. ({n} lines)"
                )),
                ("create", None) => ToolResultContent::text(format!(
                    "The file {path} has been created successfully."
                )),
                (_, Some(n)) => ToolResultContent::text(format!(
                    "The file {path} has been written successfully. ({n} lines)"
                )),
                (_, None) => ToolResultContent::text(format!(
                    "The file {path} has been written successfully."
                )),
            }
        }
        _ => {
            let raw = serde_json::to_string(value).unwrap_or_default();
            ToolResultContent::text(truncate_tool_result_content(raw))
        }
    }
}

/// WebFetch results carry the page text in `content`; hand it to the
/// model as plain text with a compact header — JSON-escaping a 20 K-char
/// page would inflate every newline and quote into escape sequences.
fn format_web_fetch_result_for_model(value: &Value) -> String {
    let Some(content) = value.get("content").and_then(Value::as_str) else {
        return serde_json::to_string(value).unwrap_or_default();
    };
    let mut header = Vec::new();
    if let Some(title) = value.get("title").and_then(Value::as_str) {
        header.push(format!("Title: {title}"));
    }
    let url = value
        .get("final_url")
        .or_else(|| value.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let status = value.get("status").and_then(Value::as_u64).unwrap_or(0);
    header.push(format!("URL: {url} (HTTP {status})"));
    if let Some(note) = value.get("note").and_then(Value::as_str) {
        header.push(format!("Note: {note}"));
    }
    let mut text = format!("{}\n\n{content}", header.join("\n"));
    if let Some(next_offset) = value.get("next_offset").and_then(Value::as_u64) {
        let total = value
            .get("total_chars")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        text.push_str(&format!(
            "\n\n[Truncated at char {next_offset} of {total} — call WebFetch again with offset={next_offset} to continue.]"
        ));
    }
    text
}

pub(super) fn read_tool_result_for_model(value: &Value, include_binary: bool) -> ToolResultContent {
    match value.get("type").and_then(Value::as_str) {
        Some("image") => read_image_tool_result_for_model(value),
        Some("pdf") => read_pdf_tool_result_for_model(value, include_binary),
        Some("parts") => read_pdf_parts_tool_result_for_model(value, include_binary),
        _ => {
            let raw = serde_json::to_string(value).unwrap_or_default();
            ToolResultContent::text(truncate_tool_result_content(raw))
        }
    }
}

pub(super) fn read_image_tool_result_for_model(value: &Value) -> ToolResultContent {
    let Some(file) = value.get("file").and_then(Value::as_object) else {
        let raw = serde_json::to_string(value).unwrap_or_default();
        return ToolResultContent::text(truncate_tool_result_content(raw));
    };
    let media_type = file
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    let Some(data) = file.get("base64").and_then(Value::as_str) else {
        let raw = serde_json::to_string(value).unwrap_or_default();
        return ToolResultContent::text(truncate_tool_result_content(raw));
    };

    ToolResultContent::blocks(vec![ToolResultContentBlock::Image(ImageBlock::base64(
        media_type, data,
    ))])
}

pub(super) fn read_pdf_tool_result_for_model(
    value: &Value,
    include_binary: bool,
) -> ToolResultContent {
    let Some(file) = value.get("file").and_then(Value::as_object) else {
        let raw = serde_json::to_string(value).unwrap_or_default();
        return ToolResultContent::text(truncate_tool_result_content(raw));
    };
    let Some(data) = file.get("base64").and_then(Value::as_str) else {
        let raw = serde_json::to_string(value).unwrap_or_default();
        return ToolResultContent::text(truncate_tool_result_content(raw));
    };
    let file_path = file
        .get("filePath")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let size = file
        .get("originalSize")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let summary = ToolResultContentBlock::Text(TextBlock {
        text: format!("PDF file read: {file_path} ({})", format_bytes(size)),
    });
    if !include_binary {
        return ToolResultContent::blocks(vec![summary]);
    }

    ToolResultContent::blocks(vec![
        summary,
        ToolResultContentBlock::Document(DocumentBlock::base64("application/pdf", data)),
    ])
}

pub(super) fn read_pdf_parts_tool_result_for_model(
    value: &Value,
    include_binary: bool,
) -> ToolResultContent {
    let Some(file) = value.get("file").and_then(Value::as_object) else {
        let raw = serde_json::to_string(value).unwrap_or_default();
        return ToolResultContent::text(truncate_tool_result_content(raw));
    };
    let file_path = file
        .get("filePath")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let size = file
        .get("originalSize")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let count = file.get("count").and_then(Value::as_u64).unwrap_or(0);
    let mut blocks = vec![ToolResultContentBlock::Text(TextBlock {
        text: format!(
            "PDF pages extracted: {count} page(s) from {file_path} ({})",
            format_bytes(size)
        ),
    })];
    if !include_binary {
        return ToolResultContent::blocks(blocks);
    }

    if let Some(pages) = file.get("pages").and_then(Value::as_array) {
        for page in pages {
            let media_type = page
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("image/jpeg");
            if let Some(data) = page.get("base64").and_then(Value::as_str) {
                blocks.push(ToolResultContentBlock::Image(ImageBlock::base64(
                    media_type, data,
                )));
            }
        }
    }

    ToolResultContent::blocks(blocks)
}

pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}
pub(super) fn format_ask_user_question_result_for_model(value: &Value) -> String {
    let answers_text = value
        .get("answers")
        .and_then(Value::as_object)
        .map(|answers| {
            answers
                .iter()
                .map(|(question_text, answer)| {
                    let answer = answer
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| answer.to_string());
                    let annotation = value
                        .get("annotations")
                        .and_then(Value::as_object)
                        .and_then(|annotations| annotations.get(question_text));

                    let mut parts = vec![format!("\"{question_text}\"=\"{answer}\"")];
                    if let Some(preview) = annotation
                        .and_then(|annotation| annotation.get("preview"))
                        .and_then(Value::as_str)
                        .filter(|preview| !preview.is_empty())
                    {
                        parts.push(format!("selected preview:\n{preview}"));
                    }
                    if let Some(notes) = annotation
                        .and_then(|annotation| annotation.get("notes"))
                        .and_then(Value::as_str)
                        .filter(|notes| !notes.is_empty())
                    {
                        parts.push(format!("user notes: {notes}"));
                    }
                    parts.join(" ")
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    format!(
        "User has answered your questions: {answers_text}. You can now continue with the user's \
         answers in mind."
    )
}

pub(super) fn format_ask_user_question_answer_for_transcript(value: &Value) -> Option<String> {
    let answers = value.get("answers").and_then(Value::as_object)?;
    if answers.is_empty() {
        return None;
    }

    let annotations = value.get("annotations").and_then(Value::as_object);
    let mut ordered_questions = Vec::new();
    if let Some(questions) = value.get("questions").and_then(Value::as_array) {
        for question in questions {
            let Some(question_text) = question
                .get("question")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
            else {
                continue;
            };
            if answers.contains_key(question_text)
                && !ordered_questions
                    .iter()
                    .any(|existing: &String| existing.as_str() == question_text)
            {
                ordered_questions.push(question_text.to_string());
            }
        }
    }

    let mut remaining = answers
        .keys()
        .filter(|question| {
            !ordered_questions
                .iter()
                .any(|existing| existing == *question)
        })
        .cloned()
        .collect::<Vec<_>>();
    remaining.sort();
    ordered_questions.extend(remaining);

    let mut lines = vec!["Answered questions:".to_string()];
    for question_text in ordered_questions {
        let Some(answer) = answers.get(&question_text) else {
            continue;
        };
        let answer = answer
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| answer.to_string());
        lines.push(format!("- {question_text}"));
        lines.push(format!("  Answer: {answer}"));

        let annotation = annotations.and_then(|annotations| annotations.get(&question_text));
        if let Some(preview) = annotation
            .and_then(|annotation| annotation.get("preview"))
            .and_then(Value::as_str)
            .filter(|preview| !preview.trim().is_empty())
        {
            lines.push("  Selected preview:".to_string());
            lines.extend(preview.lines().map(|line| format!("    {line}")));
        }
        if let Some(notes) = annotation
            .and_then(|annotation| annotation.get("notes"))
            .and_then(Value::as_str)
            .filter(|notes| !notes.trim().is_empty())
        {
            lines.push(format!("  Notes: {notes}"));
        }
    }

    Some(lines.join("\n"))
}

pub(super) fn append_permission_extra_text_to_tool_result(
    content: &mut ToolResultContent,
    extra_text: Option<&str>,
) {
    let Some(extra_text) = extra_text.map(str::trim).filter(|text| !text.is_empty()) else {
        return;
    };

    let note = format!("User note after approving this tool call: {extra_text}");
    match content {
        ToolResultContent::Text(text) => {
            if text.is_empty() {
                *text = note;
            } else {
                text.push_str("\n\n");
                text.push_str(&note);
            }
        }
        ToolResultContent::Blocks(blocks) => {
            blocks.push(ToolResultContentBlock::Text(TextBlock { text: note }));
        }
    }
}

/// Hard cap on tool-result content sent to the model. A single Read
/// of a 10 K-line file or a Bash with verbose output can easily
/// produce 200 K+ chars (~50 K tokens). Capping at 100 K chars
/// (~25 K tokens) keeps any single tool result from dominating the
/// context window while still providing enough detail for the model.
pub(super) const MAX_TOOL_RESULT_CHARS: usize = 100_000;

/// Truncate a tool-result string to [`MAX_TOOL_RESULT_CHARS`],
/// appending a note about the truncation so the model knows data
/// was lost.
pub(super) fn truncate_tool_result_content(s: String) -> String {
    if s.len() <= MAX_TOOL_RESULT_CHARS {
        return s;
    }
    let boundary = floor_char_boundary(&s, MAX_TOOL_RESULT_CHARS);
    let truncated = &s[..boundary];
    format!(
        "{truncated}\n\n[... truncated — tool result was {} chars, \
         capped at {MAX_TOOL_RESULT_CHARS} to fit context window]",
        s.len()
    )
}

/// Find the largest byte index <= `max` that is a valid char boundary.
/// Equivalent to `str::floor_char_boundary` (nightly-only).
pub(super) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub(super) fn input_str<'a>(
    input: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A tool with no projection of its own falls through to the generic
    /// path, whatever its result carries. The `ComputerUse` shape stands in
    /// for "a result the engine has no branch for": without its owning
    /// plugin's [`rebon_tool::Tool::project_result_for_model`] it is raw
    /// JSON, which is what makes the plugin-side override load-bearing.
    #[test]
    fn a_result_no_tool_projects_falls_through_to_raw_json() {
        let value = json!({
            "type": "computer_use",
            "action": "click",
            "status": "active",
            "screenshot": {
                "base64": "cG5n",
                "mediaType": "image/png",
                "width": 800,
                "height": 600,
                "scale": 2
            }
        });

        let content = compact_tool_result_for_model("ComputerUse", None, &value, None);
        let text = content.as_text().expect("no branch means raw JSON text");
        assert!(text.contains("\"action\":\"click\""), "{text}");
    }

    #[test]
    fn truncate_handles_multibyte_char_at_cut_boundary() {
        let text = "watcher  0.9.13 主要是更新了百科的头像高亮问题还有微型大炮遗物的伤害适配问题，更新后 description 就只保留最近的四次更新说明，现在有七条了太多了";

        let truncated = truncate(text, 50);

        assert!(truncated.ends_with("..."), "{truncated}");
        assert!(truncated.len() <= 50, "{truncated}");
    }

    #[test]
    fn truncate_keeps_short_multibyte_text_unchanged() {
        let text = "主要是更新";

        assert_eq!(truncate(text, 50), text);
    }

    #[test]
    fn workflow_model_result_strips_render_only_progress() {
        // `workflowProgress` is render-only telemetry for the TUI; it must
        // not leak into the model-facing tool result (which already carries
        // the script `result`).
        let value = json!({
            "status": "completed",
            "runId": "wf_demo",
            "agentCount": 3,
            "result": { "child": "done" },
            "workflowProgress": {
                "runId": "wf_demo",
                "workflowName": "demo",
                "entries": [
                    { "sequence": 1, "entry": { "type": "agent", "label": "finder" } }
                ],
            },
        });

        let content = compact_tool_result_for_model("Workflow", None, &value, None);
        let text = content.as_text().expect("workflow result is text");
        assert!(
            !text.contains("workflowProgress"),
            "model result must not contain render-only progress: {text}"
        );
        // The substantive fields the model needs are preserved.
        assert!(text.contains("\"status\":\"completed\""), "{text}");
        assert!(text.contains("\"result\""), "{text}");
        assert!(text.contains("wf_demo"), "{text}");
    }

    #[test]
    fn gateway_calls_compact_like_the_tool_they_invoke() {
        let page = json!({
            "url": "https://docs.rs/tokio",
            "status": 200,
            "title": "tokio - Rust",
            "content": "# tokio\n\nAn \"async\" runtime.\n",
            "offset": 0,
            "total_chars": 40_000,
            "truncated": true,
            "next_offset": 20_000,
        });

        let direct = compact_tool_result_for_model("WebFetch", None, &page, None)
            .as_text()
            .expect("web fetch result is text")
            .to_string();
        let gateway = compact_tool_result_for_model(
            "InvokeDeferredTool",
            Some(&json!({"tool_name": "WebFetch", "arguments": {"url": "https://docs.rs/tokio"}})),
            &page,
            None,
        )
        .as_text()
        .expect("web fetch result is text")
        .to_string();

        assert_eq!(gateway, direct);
        assert!(gateway.contains("Title: tokio - Rust"), "{gateway}");
        // Plain text, not a JSON blob with every newline escaped, and the
        // paging hint survives.
        assert!(gateway.contains("An \"async\" runtime."), "{gateway}");
        assert!(gateway.contains("offset=20000"), "{gateway}");

        // A gateway call whose input is unavailable still formats as
        // before rather than losing the result.
        let blind = compact_tool_result_for_model("InvokeDeferredTool", None, &page, None)
            .as_text()
            .expect("fallback result is text")
            .to_string();
        assert!(blind.starts_with('{'), "{blind}");
    }

    #[test]
    fn workflow_model_result_without_progress_is_unchanged() {
        let value = json!({ "status": "completed", "result": { "child": "done" } });
        let content = compact_tool_result_for_model("Workflow", None, &value, None);
        let text = content.as_text().expect("workflow result is text");
        assert!(text.contains("\"status\":\"completed\""), "{text}");
        assert!(text.contains("done"), "{text}");
    }
}
