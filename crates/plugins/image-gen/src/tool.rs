//! `ImageGen`: generate or edit one image through the provider's Images API.
//!
//! The model writes the prompt; the tool picks everything else the way codex's
//! `image_gen.imagegen` does — `gpt-image-2` with `auto` size, quality and
//! background — so there is one contract for the `imagegen` skill to teach.
//! No reference paths is a generation; one to five is an edit that sends each
//! file as a data URL. The image is saved under the generated-images
//! directory and handed back to the model as a real image block beside the
//! saved path, so a follow-up turn can copy the file or pass it back in as a
//! reference.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rebon_api::{ImageBlock, TextBlock, ToolResultContent, ToolResultContentBlock};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    parse_tool_input, require_valid_input, validation_outcome_from, ToolError, ToolId,
    ToolInputSchema, ToolProgressUpdate, ToolResult, ValidationOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::endpoint::{provider_endpoint, ImageOutput, Operation};

pub const IMAGE_GEN_TOOL_NAME: &str = "ImageGen";

/// The image model every request names, as codex's tool does. The skill's
/// guidance (sizes, transparency, fidelity) is written for this model.
const IMAGE_MODEL: &str = "gpt-image-2";

/// The most reference images one edit sends, as codex's tool allows.
const MAX_REFERENCED_IMAGES: usize = 5;

/// The Images API refuses inputs of 50 MB or more; refusing here saves an
/// upload the server would reject.
const MAX_REFERENCED_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

/// How often a running request reports that it is still running. Generation
/// takes minutes, and a tool row that says nothing for that long reads as
/// hung.
const HEARTBEAT: Duration = Duration::from_secs(15);

const DESCRIPTION: &str = include_str!("tool_description.md");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageGenInput {
    prompt: String,
    #[serde(default)]
    referenced_image_paths: Vec<PathBuf>,
}

pub struct ImageGenTool;

impl ImageGenTool {
    fn prepare_input(&self, input: &Value, context: &ToolContext) -> ToolResult<ImageGenInput> {
        let parsed: ImageGenInput = parse_tool_input(self.id(), input)?;
        require_valid_input(
            self.id(),
            validate_parsed_input(&parsed),
            "ImageGen input is invalid",
        )?;
        for path in &parsed.referenced_image_paths {
            rebon_tool::path_scope::enforce_read_path_policy(
                self.id(),
                context,
                path,
                "referenced_image_paths",
            )?;
        }
        Ok(parsed)
    }

    fn execution_error(&self, message: impl Into<String>) -> ToolError {
        ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!(message.into()),
        }
    }
}

fn validate_parsed_input(input: &ImageGenInput) -> ValidationOutcome {
    if input.prompt.trim().is_empty() {
        return ValidationOutcome::invalid("prompt must not be empty", 400);
    }
    if input.referenced_image_paths.len() > MAX_REFERENCED_IMAGES {
        return ValidationOutcome::invalid(
            format!("referenced_image_paths takes at most {MAX_REFERENCED_IMAGES} paths"),
            400,
        );
    }
    if let Some(path) = input
        .referenced_image_paths
        .iter()
        .find(|path| !path.is_absolute())
    {
        return ValidationOutcome::invalid(
            format!(
                "referenced_image_paths must be absolute; got {}",
                path.display()
            ),
            400,
        );
    }
    ValidationOutcome::valid()
}

#[async_trait]
impl Tool for ImageGenTool {
    fn id(&self) -> ToolId {
        ToolId::new(IMAGE_GEN_TOOL_NAME)
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The full image prompt: what to generate, or what to change in the referenced images and what to keep."
                },
                "referenced_image_paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": MAX_REFERENCED_IMAGES,
                    "description": "Absolute paths of up to 5 local images to edit or use as references. Omit to generate a new image."
                }
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }

    /// On the model's list only while the adopted provider has an Images
    /// endpoint — a first-party OpenAI route.
    fn is_enabled(&self) -> bool {
        provider_endpoint().is_some()
    }

    fn search_hint(&self) -> Option<&str> {
        Some("image generation edit picture illustration photo sprite texture mockup gpt-image")
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(self.prepare_input(input, context))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let Some(endpoint) = provider_endpoint() else {
            return Err(self.execution_error(
                "image generation is unavailable: the active provider is not a first-party OpenAI route",
            ));
        };
        let parsed = self.prepare_input(&input, context)?;
        let (operation, body) = request_body(&parsed).map_err(|err| self.execution_error(err))?;

        let started = Instant::now();
        let request = endpoint.send(operation, &body);
        tokio::pin!(request);
        let mut heartbeat =
            tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
        let output = loop {
            tokio::select! {
                result = &mut request => break result,
                _ = heartbeat.tick() => {
                    context.emit_progress(
                        ToolProgressUpdate::new("image_generation")
                            .with_message(format!(
                                "Generating image… {}s",
                                started.elapsed().as_secs()
                            )),
                    );
                }
            }
        }
        .map_err(|err| self.execution_error(err))?;

        Ok(result_value(&parsed, output, context))
    }

    /// A generated image is worth nothing to the model as a path in JSON: it
    /// has to see the pixels to check the result against the prompt. Hand it
    /// over as a summary line plus the image, read back from where it was
    /// saved — or from the result itself when saving failed.
    fn project_result_for_model(&self, value: &Value) -> Option<ToolResultContent> {
        let media_type = value.get("media_type").and_then(Value::as_str)?;
        let summary = value.get("summary").and_then(Value::as_str)?;
        let data = match value.get("image_base64").and_then(Value::as_str) {
            Some(inline) => inline.to_string(),
            None => {
                let path = value.get("file_path").and_then(Value::as_str)?;
                match std::fs::read(path) {
                    Ok(bytes) => BASE64.encode(bytes),
                    Err(err) => {
                        return Some(ToolResultContent::text(format!(
                            "{summary}\nThe saved image could not be read back ({err})."
                        )))
                    }
                }
            }
        };
        Some(ToolResultContent::blocks(vec![
            ToolResultContentBlock::Text(TextBlock {
                text: summary.to_string(),
            }),
            ToolResultContentBlock::Image(ImageBlock::base64(media_type, data)),
        ]))
    }
}

/// The endpoint and JSON body for one call: a generation without references,
/// an edit with them.
fn request_body(input: &ImageGenInput) -> Result<(Operation, Value), String> {
    let mut body = json!({
        "prompt": input.prompt,
        "model": IMAGE_MODEL,
        "background": "auto",
        "quality": "auto",
        "size": "auto",
    });
    if input.referenced_image_paths.is_empty() {
        return Ok((Operation::Generate, body));
    }
    let images = input
        .referenced_image_paths
        .iter()
        .map(|path| image_data_url(path).map(|url| json!({ "image_url": url })))
        .collect::<Result<Vec<_>, _>>()?;
    body["images"] = Value::Array(images);
    Ok((Operation::Edit, body))
}

fn image_data_url(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|err| format!("cannot read referenced image {}: {err}", path.display()))?
        .len();
    if size >= MAX_REFERENCED_IMAGE_BYTES {
        return Err(format!(
            "referenced image {} is {size} bytes; the Images API takes images under 50 MB",
            path.display()
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|err| format!("cannot read referenced image {}: {err}", path.display()))?;
    let media_type = sniff_media_type(&bytes).ok_or_else(|| {
        format!(
            "referenced image {} is not a PNG, JPEG, WebP or GIF file",
            path.display()
        )
    })?;
    Ok(format!("data:{media_type};base64,{}", BASE64.encode(bytes)))
}

/// The image format by its signature, for the formats the Images API reads.
/// The extension is not trusted: a renamed file is a common way to get this
/// wrong, and the server's error for it names no file.
fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

/// The tool's result: where the image was saved and what to tell the model.
///
/// The bytes stay out of the JSON when the file was written — the transcript
/// and every ACP client get a path, and the model gets the pixels through
/// [`ImageGenTool::project_result_for_model`]. Only a failed save carries the
/// image inline, so a generation that took minutes is not lost to a full disk.
fn result_value(input: &ImageGenInput, output: ImageOutput, context: &ToolContext) -> Value {
    let bytes = BASE64.decode(output.b64_json.trim());
    let media_type = bytes
        .as_deref()
        .ok()
        .and_then(sniff_media_type)
        .unwrap_or("image/png");
    let saved = bytes
        .map_err(|err| format!("the image data was not valid base64: {err}"))
        .and_then(|bytes| save_image(&bytes, media_type, context));

    let mut value = json!({
        "prompt": input.prompt,
        "media_type": media_type,
        "operation": if input.referenced_image_paths.is_empty() { "generate" } else { "edit" },
    });
    if let Some(background) = &output.background {
        value["background"] = json!(background);
    }
    if let Some(revised) = &output.revised_prompt {
        value["revised_prompt"] = json!(revised);
    }
    match saved {
        Ok(path) => {
            let path = path.display().to_string();
            value["summary"] = json!(saved_summary(&path));
            value["file_path"] = json!(path);
        }
        Err(err) => {
            tracing::warn!("generated image not saved: {err}");
            value["summary"] = json!(format!(
                "The image was generated but could not be saved ({err}). It is shown below only; save it again if it is needed on disk."
            ));
            value["save_error"] = json!(err);
            value["image_base64"] = json!(output.b64_json.trim());
        }
    }
    value
}

fn saved_summary(path: &str) -> String {
    format!(
        "Generated image saved to {path}.\n\
         If you need it at another path, copy it there and leave the original in place unless the user explicitly asks you to delete it.\n\
         The image is already shown to the user; do not render it again as a Markdown image or file link."
    )
}

/// Write the image as `<generated-images>/<session>/<tool-use-id>.<ext>`.
///
/// The base directory is `generatedImagesDir` resolved against the session's
/// working directory — the same place the hosted image path used, so images
/// from before and after this tool sit side by side. An existing file is
/// never overwritten: ids are unique per call, so one being there means
/// something else wrote it.
fn save_image(bytes: &[u8], media_type: &str, context: &ToolContext) -> Result<PathBuf, String> {
    let cwd = context.cwd().ok_or_else(|| {
        "the tool call has no working directory to resolve the image directory against".to_string()
    })?;
    let dir = rebon_config::generated_images_output_base(Path::new(cwd)).join(sanitize_segment(
        context.session_id().unwrap_or_default(),
        "session",
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let path = dir.join(format!(
        "{}.{}",
        sanitize_segment(context.tool_use_id().unwrap_or_default(), "image"),
        extension_for(media_type)
    ));
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    file.write_all(bytes)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    Ok(path)
}

/// The file extension for a media type [`sniff_media_type`] can return.
fn extension_for(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// An id as one path segment: anything but ASCII letters, digits, `-` and
/// `_` becomes `_`, and an empty id takes `fallback`.
fn sanitize_segment(raw: &str, fallback: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
