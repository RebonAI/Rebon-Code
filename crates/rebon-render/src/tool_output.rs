//! Derivation helpers that project a raw tool-output JSON value into the
//! structured ACP shapes a client renders — [`ToolCallContent`] and
//! [`ToolCallLocation`].
//!
//! Two code paths consume these:
//! * A live turn, right after a tool finishes, to synthesize the
//!   `SessionUpdate::ToolCallUpdate` payload.
//! * Transcript replay, to reconstitute the same payload from the
//!   `toolUseResults` map persisted alongside each batched user tool_result
//!   entry — so a resumed session can render the Edit / Write diff block the
//!   user saw live.
//!
//! Both are projections from tool results and progress to what a client draws,
//! which is why they live in the crate that owns projections.

use rebon_types::{
    ContentBlock, DiffContent, ImageContent, RegularContent, TextContent, ToolCallContent,
    ToolCallLocation,
};
use serde_json::Value;

fn image_result_file_path(tool_use_result: &Value) -> Option<&str> {
    if tool_use_result.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let file = tool_use_result.get("file").and_then(Value::as_object)?;
    let media_type = file.get("type").and_then(Value::as_str)?;
    if !media_type.starts_with("image/") {
        return None;
    }
    file.get("filePath").and_then(Value::as_str)
}

fn trimmed_image_result_for_transcript(tool_use_result: &Value) -> Option<Value> {
    let path = image_result_file_path(tool_use_result)?;
    let media_type = tool_use_result
        .get("file")
        .and_then(Value::as_object)?
        .get("type")
        .and_then(Value::as_str)?;
    Some(serde_json::json!({
        "type": "image",
        "file": {
            "filePath": path,
            "type": media_type,
        }
    }))
}

fn trimmed_pdf_result_for_transcript(tool_use_result: &Value) -> Option<Value> {
    if tool_use_result.get("type").and_then(Value::as_str) != Some("pdf") {
        return None;
    }
    let file = tool_use_result.get("file").and_then(Value::as_object)?;
    let path = file.get("filePath").and_then(Value::as_str)?;
    let original_size = file
        .get("originalSize")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(serde_json::json!({
        "type": "pdf",
        "file": {
            "filePath": path,
            "originalSize": original_size,
        }
    }))
}

fn trimmed_pdf_parts_result_for_transcript(tool_use_result: &Value) -> Option<Value> {
    if tool_use_result.get("type").and_then(Value::as_str) != Some("parts") {
        return None;
    }
    let file = tool_use_result.get("file").and_then(Value::as_object)?;
    let path = file.get("filePath").and_then(Value::as_str)?;
    let original_size = file
        .get("originalSize")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let count = file.get("count").and_then(Value::as_u64).unwrap_or(0);
    let output_dir = file.get("outputDir").and_then(Value::as_str).unwrap_or("");
    Some(serde_json::json!({
        "type": "parts",
        "file": {
            "filePath": path,
            "originalSize": original_size,
            "count": count,
            "outputDir": output_dir,
        }
    }))
}

/// Extract file-system anchors from a raw tool-output JSON.
///
/// A tool may publish its locations as `filenames[]`, `file_path`, `file`,
/// `filePath`, or `file.filePath`. The first present wins; if nothing
/// fits, returns `None`.
pub fn extract_locations(tool_use_result: &Value) -> Option<Vec<ToolCallLocation>> {
    let result = tool_use_result.as_object()?;
    if image_result_file_path(tool_use_result).is_some() {
        return None;
    }

    if let Some(filenames) = result.get("filenames").and_then(Value::as_array) {
        let locations: Vec<ToolCallLocation> = filenames
            .iter()
            .filter_map(Value::as_str)
            .map(|path| ToolCallLocation {
                path: path.to_string(),
                line: None,
            })
            .collect();
        if !locations.is_empty() {
            return Some(locations);
        }
    }
    if let Some(file_path) = result.get("file_path").and_then(Value::as_str) {
        return Some(vec![ToolCallLocation {
            path: file_path.to_string(),
            line: None,
        }]);
    }
    if let Some(file) = result.get("file").and_then(Value::as_str) {
        return Some(vec![ToolCallLocation {
            path: file.to_string(),
            line: None,
        }]);
    }
    if let Some(file) = result.get("filePath").and_then(Value::as_str) {
        return Some(vec![ToolCallLocation {
            path: file.to_string(),
            line: None,
        }]);
    }
    if let Some(file) = result
        .get("file")
        .and_then(Value::as_object)
        .and_then(|file| file.get("filePath"))
        .and_then(Value::as_str)
    {
        return Some(vec![ToolCallLocation {
            path: file.to_string(),
            line: None,
        }]);
    }
    None
}

/// Build a readable summary for the Skill tool's structured result.
///
/// `Skill` returns the expanded prompt as JSON so the model can execute it in
/// the next turn. Showing the raw `prompt` field alone makes the transcript look
/// like a plain assistant message; this summary keeps the skill lifecycle
/// visible as a tool result.
fn skill_tool_result_summary(tool_use_result: &Value) -> Option<String> {
    let skill = tool_use_result.get("skill").and_then(Value::as_str)?;
    let prompt = tool_use_result.get("prompt").and_then(Value::as_str)?;

    let mut lines = vec![format!("Loaded skill /{skill}")];
    if let Some(title) = tool_use_result
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && *s != skill)
    {
        lines.push(format!("Title: {title}"));
    }
    if let Some(description) = tool_use_result
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        lines.push(format!("Description: {description}"));
    }
    if let Some(tools) = tool_use_result
        .get("suggested_tools")
        .and_then(Value::as_array)
    {
        let tools = tools
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            lines.push(format!("Suggested tools: {}", tools.join(", ")));
        }
    }
    if !prompt.trim().is_empty() {
        lines.push("Expanded instructions:".to_string());
        lines.extend(prompt.lines().map(str::to_string));
    }

    Some(lines.join("\n"))
}

// Kept on the content block, not raw_output: streaming patches retain every
// content block but replace raw_output with each incoming progress payload.
const TOOL_PROGRESS_ANNOTATION: &str = "rebonToolProgress";

/// Project progress text without throwing away its existing kind and payload.
/// Plain-text clients still see the same message; renderers can distinguish
/// execution activity from final output without parsing the displayed prose.
pub fn tool_progress_update_content(
    progress: &rebon_tools_core::ToolProgressUpdate,
) -> Option<Vec<ToolCallContent>> {
    let message = progress.message.as_ref()?;
    Some(vec![ToolCallContent::Content(RegularContent {
        content: ContentBlock::Text(TextContent {
            text: message.clone(),
            annotations: Some(serde_json::json!({
                TOOL_PROGRESS_ANNOTATION: {
                    "kind": progress.kind,
                    "payload": progress.payload,
                }
            })),
        }),
    })])
}

/// Metadata written by [`tool_progress_update_content`], absent on final output
/// and on older content whose progress/result boundary was not retained.
pub fn tool_progress_metadata(content: &ToolCallContent) -> Option<&Value> {
    let ToolCallContent::Content(RegularContent {
        content: ContentBlock::Text(text),
    }) = content
    else {
        return None;
    };
    text.annotations.as_ref()?.get(TOOL_PROGRESS_ANNOTATION)
}

/// Project a raw tool-output JSON into the ACP `ToolCallContent` list a
/// client uses to decide whether to render a diff, a terminal, or plain
/// text.
///
/// The diff shape is keyed off `filePath` + `newString` (Edit / Write /
/// MultiEdit share it), with an `oldString` present only on real
/// edits (absent or empty on Write-create). Non-diff tools return plain text
/// directly or fall through to the `file.content` / `content` / `stdout` /
/// `prompt` ladder.
pub fn tool_result_update_content(tool_use_result: &Value) -> Option<Vec<ToolCallContent>> {
    // run_code returns its collected console logs and return value as a string.
    // Object-only projection silently dropped that final output at completion.
    if let Some(text) = tool_use_result.as_str() {
        return (!text.is_empty()).then(|| vec![text_content(text.to_string())]);
    }
    if tool_use_result.get("type").and_then(Value::as_str) == Some("computer_use") {
        let screenshot = tool_use_result.get("screenshot")?.as_object()?;
        let data = screenshot.get("base64")?.as_str()?;
        let media_type = screenshot
            .get("mediaType")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        let action = tool_use_result
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("action");
        let width = screenshot.get("width").and_then(Value::as_u64).unwrap_or(0);
        let height = screenshot
            .get("height")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return Some(vec![
            text_content(format!(
                "ComputerUse {action} completed; target screenshot {width}x{height}."
            )),
            ToolCallContent::Content(RegularContent {
                content: ContentBlock::Image(ImageContent {
                    mime_type: media_type.to_string(),
                    data: data.to_string(),
                    uri: None,
                    annotations: None,
                }),
            }),
        ]);
    }

    if let Some(display_message) = tool_use_result
        .get("errorPresentation")
        .and_then(Value::as_object)
        .and_then(|presentation| presentation.get("displayMessage"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
    {
        return Some(vec![text_content(display_message.to_string())]);
    }

    let mut memory_notification = tool_use_result
        .get("memoryNotification")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let (Some(file_path), Some(new_string)) = (
        tool_use_result.get("filePath").and_then(Value::as_str),
        tool_use_result.get("newString").and_then(Value::as_str),
    ) {
        let old_text = tool_use_result
            .get("oldString")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut content = vec![ToolCallContent::Diff(DiffContent {
            path: file_path.to_string(),
            old_text,
            new_text: new_string.to_string(),
        })];
        if let Some(notification) = memory_notification {
            content.push(text_content(notification));
        }
        return Some(content);
    }

    let text = if let Some(summary) = skill_tool_result_summary(tool_use_result) {
        Some(summary)
    } else if let Some(path) = image_result_file_path(tool_use_result) {
        Some(format!("Image file: {path}"))
    } else if let Some(file) = tool_use_result.get("file").and_then(Value::as_object) {
        file.get("content")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else if let Some(content) = tool_use_result.get("content").and_then(Value::as_str) {
        Some(content.to_string())
    } else if let Some(stdout) = tool_use_result.get("stdout").and_then(Value::as_str) {
        Some(stdout.to_string())
    } else {
        tool_use_result
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let mut content = Vec::new();
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        if memory_notification.as_deref() == Some(text.as_str()) {
            memory_notification = None;
        }
        content.push(text_content(text));
    }
    if let Some(notification) = memory_notification {
        content.push(text_content(notification));
    }

    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

fn text_content(text: String) -> ToolCallContent {
    ToolCallContent::Content(RegularContent {
        content: ContentBlock::Text(TextContent {
            text,
            annotations: None,
        }),
    })
}

/// Shrink a tool's raw output before persisting it into the on-disk
/// transcript under `toolUseResults`.
///
/// Most tools return small structured payloads that are safe to keep
/// unchanged so replay can rebuild `ToolCallContent::Diff`. `Read` is
/// a notable exception: it includes the whole file body under
/// `file.content` (or top-level `content`). `Edit` and `Write` also
/// return the complete updated file as top-level `content` alongside
/// `originalFile`, `oldString`, and `newString`; the updated-file copy
/// is redundant for replay and diff rendering. We drop those large text
/// bodies while retaining the structural fields those paths consume.
///
/// Unknown tool names pass through — the caller controls what names
/// take a trim path.
pub fn trim_raw_output_for_transcript(tool_name: &str, value: Value) -> Value {
    let trim_read = matches!(tool_name, "Read");
    let trim_updated_file = matches!(
        tool_name,
        "Edit" | "Write" | "MultiEdit" | "FileEditTool" | "FileWriteTool" | "MultiEditTool"
    );
    if !trim_read && !trim_updated_file {
        return value;
    }
    if trim_read {
        if let Some(trimmed_image) = trimmed_image_result_for_transcript(&value) {
            return trimmed_image;
        }
        if let Some(trimmed_pdf) = trimmed_pdf_result_for_transcript(&value) {
            return trimmed_pdf;
        }
        if let Some(trimmed_pdf_parts) = trimmed_pdf_parts_result_for_transcript(&value) {
            return trimmed_pdf_parts;
        }
    }

    let Value::Object(mut map) = value else {
        return value;
    };
    if trim_read {
        if let Some(Value::Object(file_map)) = map.get_mut("file") {
            file_map.remove("content");
        }
    }
    map.remove("content");
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progress_content_preserves_metadata_without_duplicating_its_message() {
        let progress = rebon_tools_core::ToolProgressUpdate::new("code_mode/dispatch")
            .with_message("← Bash succeeded")
            .with_payload(json!({"seq": 2, "tool": "Bash", "isError": false}));
        let content = tool_progress_update_content(&progress).expect("progress text");
        assert_eq!(content.len(), 1);
        assert_eq!(
            crate::content::render_tool_call_content(&content[0]),
            "← Bash succeeded"
        );
        let metadata = tool_progress_metadata(&content[0]).expect("structured progress");
        assert_eq!(metadata["kind"], progress.kind);
        assert_eq!(metadata["payload"], progress.payload.unwrap());
        assert!(metadata.get("message").is_none());
        assert!(tool_progress_metadata(&text_content("← Bash succeeded".into())).is_none());
        assert!(
            tool_progress_update_content(&rebon_tools_core::ToolProgressUpdate::new(
                "without-message"
            ))
            .is_none()
        );
        let roundtrip: Vec<ToolCallContent> =
            serde_json::from_value(serde_json::to_value(&content).expect("serialize progress"))
                .expect("deserialize progress");
        assert_eq!(tool_progress_metadata(&roundtrip[0]), Some(metadata));
    }

    #[test]
    fn string_tool_results_preserve_all_console_output_and_return_value() {
        for output in [
            "first console output\nsecond console output\nreturned value",
            "(run_code completed with no output)",
        ] {
            assert_eq!(
                tool_result_update_content(&json!(output)),
                Some(vec![text_content(output.to_string())]),
                "run_code returns a JSON string, not an object with a content field"
            );
        }
        assert!(tool_result_update_content(&json!("")).is_none());
    }

    #[test]
    fn computer_use_update_contains_summary_and_image() {
        let value = json!({
            "type": "computer_use",
            "action": "click",
            "screenshot": {
                "base64": "aW1hZ2U=",
                "mediaType": "image/png",
                "width": 800,
                "height": 600
            }
        });
        let content = tool_result_update_content(&value).unwrap();
        assert_eq!(content.len(), 2);
        let ToolCallContent::Content(RegularContent {
            content: ContentBlock::Image(image),
        }) = &content[1]
        else {
            panic!("expected image content");
        };
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, "aW1hZ2U=");
    }

    #[test]
    fn update_content_uses_structured_error_display_message() {
        let value = json!({
            "errorPresentation": {
                "code": "agent_closed",
                "displayMessage": "Agent is no longer available."
            }
        });

        let content = tool_result_update_content(&value).unwrap();
        let ToolCallContent::Content(RegularContent {
            content: ContentBlock::Text(text),
        }) = &content[0]
        else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "Agent is no longer available.");
    }

    #[test]
    fn extract_locations_prefers_filenames_array() {
        let v = json!({"filenames": ["a.rs", "b.rs"], "file_path": "c.rs"});
        let got = extract_locations(&v).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, "a.rs");
        assert_eq!(got[1].path, "b.rs");
    }

    #[test]
    fn extract_locations_falls_through_empty_filenames() {
        let v = json!({"filenames": [], "file_path": "c.rs"});
        let got = extract_locations(&v).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "c.rs");
    }

    #[test]
    fn extract_locations_reads_camelcase_file_path() {
        let v = json!({"filePath": "/tmp/x"});
        let got = extract_locations(&v).unwrap();
        assert_eq!(got[0].path, "/tmp/x");
    }

    #[test]
    fn extract_locations_reads_nested_file_filepath() {
        let v = json!({"file": {"filePath": "/tmp/x"}});
        let got = extract_locations(&v).unwrap();
        assert_eq!(got[0].path, "/tmp/x");
    }

    #[test]
    fn extract_locations_none_when_no_fields_match() {
        assert!(extract_locations(&json!({"unrelated": true})).is_none());
    }

    #[test]
    fn update_content_edit_tool_builds_diff_with_old_and_new() {
        let v = json!({
            "filePath": "/tmp/a.rs",
            "oldString": "before",
            "newString": "after",
        });
        let content = tool_result_update_content(&v).unwrap();
        match &content[0] {
            ToolCallContent::Diff(d) => {
                assert_eq!(d.path, "/tmp/a.rs");
                assert_eq!(d.old_text.as_deref(), Some("before"));
                assert_eq!(d.new_text, "after");
            }
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn update_content_write_tool_create_has_no_old_text() {
        let v = json!({
            "filePath": "/tmp/new.rs",
            "oldString": "",
            "newString": "fresh content",
        });
        let content = tool_result_update_content(&v).unwrap();
        match &content[0] {
            ToolCallContent::Diff(d) => {
                assert!(
                    d.old_text.is_none(),
                    "empty oldString must normalise to None"
                );
                assert_eq!(d.new_text, "fresh content");
            }
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn update_content_preserves_diff_and_surfaces_memory_notification() {
        let v = json!({
            "filePath": "/tmp/memory/MEMORY.md",
            "oldString": "before",
            "newString": "after",
            "memoryNotification": "Memory updated in ./MEMORY.md \u{00B7} /memory to edit",
        });
        let content = tool_result_update_content(&v).unwrap();
        assert_eq!(content.len(), 2);
        match &content[0] {
            ToolCallContent::Diff(d) => {
                assert_eq!(d.path, "/tmp/memory/MEMORY.md");
                assert_eq!(d.new_text, "after");
            }
            other => panic!("expected Diff, got {other:?}"),
        }
        match &content[1] {
            ToolCallContent::Content(regular) => match &regular.content {
                ContentBlock::Text(t) => assert_eq!(
                    t.text,
                    "Memory updated in ./MEMORY.md \u{00B7} /memory to edit"
                ),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn update_content_surfaces_memory_notification_without_other_content() {
        let v = json!({
            "memoryNotification": "Memory updated in ./user.md \u{00B7} /memory to edit",
        });
        let content = tool_result_update_content(&v).unwrap();
        match &content[0] {
            ToolCallContent::Content(regular) => match &regular.content {
                ContentBlock::Text(t) => assert_eq!(
                    t.text,
                    "Memory updated in ./user.md \u{00B7} /memory to edit"
                ),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn update_content_summarizes_skill_tool_result() {
        let v = json!({
            "skill": "imagegen",
            "title": "Image generation",
            "description": "Generate an image",
            "prompt": "Use image_gen to draw a fox.\nReport the saved path.",
            "suggested_tools": ["image_gen"]
        });
        let content = tool_result_update_content(&v).unwrap();
        match &content[0] {
            ToolCallContent::Content(regular) => match &regular.content {
                ContentBlock::Text(t) => {
                    assert!(t.text.contains("Loaded skill /imagegen"));
                    assert!(t.text.contains("Title: Image generation"));
                    assert!(t.text.contains("Suggested tools: image_gen"));
                    assert!(t.text.contains("Expanded instructions:"));
                    assert!(t.text.contains("Use image_gen to draw a fox."));
                }
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn extract_locations_omits_read_image_path() {
        let v = json!({
            "type": "image",
            "file": {
                "filePath": "/tmp/pixel.jpg",
                "base64": "abc",
                "type": "image/jpeg"
            }
        });
        assert!(extract_locations(&v).is_none());
    }

    #[test]
    fn update_content_read_image_surfaces_single_summary() {
        let v = json!({
            "type": "image",
            "file": {
                "filePath": "/tmp/pixel.jpg",
                "base64": "abc",
                "type": "image/jpeg"
            }
        });
        let content = tool_result_update_content(&v).unwrap();
        assert_eq!(content.len(), 1);
        match &content[0] {
            ToolCallContent::Content(regular) => match &regular.content {
                ContentBlock::Text(t) => assert_eq!(t.text, "Image file: /tmp/pixel.jpg"),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn trim_read_image_removes_base64_but_keeps_summary_fields() {
        let v = json!({
            "type": "image",
            "file": {
                "filePath": "/tmp/pixel.jpg",
                "base64": "abc",
                "type": "image/jpeg"
            }
        });
        let trimmed = trim_raw_output_for_transcript("Read", v);
        assert_eq!(trimmed["type"], "image");
        assert_eq!(trimmed["file"]["filePath"], "/tmp/pixel.jpg");
        assert_eq!(trimmed["file"]["type"], "image/jpeg");
        assert!(trimmed["file"].get("base64").is_none());
    }

    #[test]
    fn update_content_read_tool_surfaces_file_content() {
        let v = json!({"file": {"content": "hello"}});
        let content = tool_result_update_content(&v).unwrap();
        match &content[0] {
            ToolCallContent::Content(regular) => match &regular.content {
                ContentBlock::Text(t) => assert_eq!(t.text, "hello"),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn update_content_falls_back_to_stdout() {
        let v = json!({"stdout": "ran"});
        let content = tool_result_update_content(&v).unwrap();
        match &content[0] {
            ToolCallContent::Content(regular) => match &regular.content {
                ContentBlock::Text(t) => assert_eq!(t.text, "ran"),
                other => panic!("expected Text, got {other:?}"),
            },
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn update_content_returns_none_when_text_is_empty() {
        let v = json!({"content": ""});
        assert!(tool_result_update_content(&v).is_none());
    }

    #[test]
    fn update_content_returns_none_for_unknown_shape() {
        assert!(tool_result_update_content(&json!({"nothing": 1})).is_none());
    }

    #[test]
    fn trim_removes_read_file_body_but_keeps_structure() {
        let v = json!({
            "file": {"filePath": "/tmp/x", "content": "LONG TEXT"},
            "type": "text",
            "numLines": 5,
        });
        let trimmed = trim_raw_output_for_transcript("Read", v);
        let obj = trimmed.as_object().unwrap();
        let file = obj.get("file").unwrap().as_object().unwrap();
        assert!(!file.contains_key("content"));
        assert_eq!(file.get("filePath").unwrap(), "/tmp/x");
        assert_eq!(obj.get("numLines").unwrap(), 5);
    }

    #[test]
    fn trim_removes_top_level_read_content() {
        let v = json!({"filePath": "/tmp/x", "content": "BIG"});
        let trimmed = trim_raw_output_for_transcript("Read", v);
        assert!(trimmed.get("content").is_none());
        assert_eq!(trimmed.get("filePath").unwrap(), "/tmp/x");
    }

    /// MultiEdit and NotebookEdit deliberately emit the same
    /// `filePath` + `oldString` + `newString` shape as Edit, which is
    /// what lets them reuse the diff card without any tool-name-keyed
    /// change in any renderer. Pin that, because the shape is the whole
    /// contract.
    #[test]
    fn multi_edit_and_notebook_edit_results_render_as_diffs() {
        let multi = tool_result_update_content(&json!({
            "type": "update",
            "filePath": "/repo/src/lib.rs",
            "oldString": "old span\n",
            "newString": "new span\n",
            "editCount": 2,
            "replacements": 2,
        }))
        .expect("MultiEdit result should produce renderable content");
        match multi.as_slice() {
            [ToolCallContent::Diff(diff)] => {
                assert_eq!(diff.path, "/repo/src/lib.rs");
                assert_eq!(diff.old_text.as_deref(), Some("old span\n"));
                assert_eq!(diff.new_text, "new span\n");
            }
            other => panic!("expected a single diff, got {other:?}"),
        }

        let notebook = tool_result_update_content(&json!({
            "type": "update",
            "filePath": "/repo/analysis.ipynb",
            "cellId": "compute",
            "editMode": "replace",
            "oldString": "print(6 * 7)\n",
            "newString": "print('replaced')\n",
        }))
        .expect("NotebookEdit result should produce renderable content");
        match notebook.as_slice() {
            [ToolCallContent::Diff(diff)] => {
                assert_eq!(diff.path, "/repo/analysis.ipynb");
                assert_eq!(diff.old_text.as_deref(), Some("print(6 * 7)\n"));
                assert_eq!(diff.new_text, "print('replaced')\n");
            }
            other => panic!("expected a single diff, got {other:?}"),
        }
    }

    /// Insert mode has no previous source, so the card must degrade to a
    /// pure addition rather than losing the diff entirely.
    #[test]
    fn notebook_insert_result_renders_as_an_addition_only_diff() {
        let content = tool_result_update_content(&json!({
            "type": "update",
            "filePath": "/repo/analysis.ipynb",
            "editMode": "insert",
            "oldString": "",
            "newString": "import os\n",
        }))
        .expect("insert result should produce renderable content");
        match content.as_slice() {
            [ToolCallContent::Diff(diff)] => {
                assert_eq!(diff.old_text, None);
                assert_eq!(diff.new_text, "import os\n");
            }
            other => panic!("expected a single diff, got {other:?}"),
        }
    }

    #[test]
    fn trim_removes_edit_and_write_content_but_keeps_diff_fields() {
        for tool_name in [
            "Edit",
            "Write",
            "MultiEdit",
            "FileEditTool",
            "FileWriteTool",
        ] {
            let v = json!({
                "filePath": "/tmp/x",
                "oldString": "before",
                "newString": "after",
                "content": "COMPLETE UPDATED FILE",
                "originalFile": "COMPLETE ORIGINAL FILE",
                "numLines": 10,
                "memoryNotification": "remember this"
            });
            let trimmed = trim_raw_output_for_transcript(tool_name, v);
            assert!(trimmed.get("content").is_none(), "tool={tool_name}");
            assert_eq!(trimmed["filePath"], "/tmp/x");
            assert_eq!(trimmed["oldString"], "before");
            assert_eq!(trimmed["newString"], "after");
            assert_eq!(trimmed["originalFile"], "COMPLETE ORIGINAL FILE");
            assert_eq!(trimmed["numLines"], 10);
            assert_eq!(trimmed["memoryNotification"], "remember this");
        }
    }

    #[test]
    fn trim_is_noop_for_unrelated_tools() {
        let v = json!({"content": "keep", "stdout": "ran"});
        let trimmed = trim_raw_output_for_transcript("Bash", v.clone());
        assert_eq!(trimmed, v);
    }

    #[test]
    fn trim_tolerates_non_object_values() {
        let v = json!([1, 2, 3]);
        let trimmed = trim_raw_output_for_transcript("Read", v.clone());
        assert_eq!(trimmed, v);
    }
}
