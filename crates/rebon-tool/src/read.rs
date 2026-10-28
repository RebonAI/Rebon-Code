use crate::{Tool, ToolContext};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use image::{
    io::{Limits, Reader as ImageReader},
    AnimationDecoder, ImageDecoder, ImageFormat,
};
use rebon_tools_core::{
    file_state::{file_mtime_ms, FileState},
    require_valid_input, validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use serde_json::{json, Value};
use std::fs;
use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;

const FILE_READ_TOOL_NAME: &str = "Read";
const MAX_READ_TARGETS: usize = 16;
pub const READ_BATCH_TEXT_BUDGET: usize = 32 * 1024;
pub const READ_BATCH_NOTICE_BUDGET: usize = 8192;
const READ_BATCH_BINARY_BUDGET: usize = 28 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReadBatchDelivery {
    Full,
    Partial,
    Omitted,
}

pub fn read_batch_delivery(results: &[Value], include_binary: bool) -> Vec<ReadBatchDelivery> {
    // Conservative size estimates keep this decision identical for the cache
    // and model projection without requiring the raw file content in the cache
    // to pass through the model-facing representation.
    let mut text = 0usize;
    let mut binary = 0usize;
    let mut stopped = false;
    let mut delivery = Vec::with_capacity(results.len());
    for result in results {
        if stopped {
            delivery.push(ReadBatchDelivery::Omitted);
            continue;
        }
        let path = result
            .get("filePath")
            .and_then(Value::as_str)
            .or_else(|| {
                result
                    .get("file")
                    .and_then(|f| f.get("filePath"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("<unknown>");
        let file = result.get("file").and_then(Value::as_object);
        let kind = result.get("type").and_then(Value::as_str);
        let mut item_text = "File: ".len().saturating_add(path.len());
        let mut item_binary = 0usize;
        match (kind, file) {
            (Some("image"), Some(file)) if file.get("base64").and_then(Value::as_str).is_some() => {
                item_binary = file["base64"].as_str().unwrap().len();
            }
            (Some("pdf"), Some(file)) if file.get("base64").and_then(Value::as_str).is_some() => {
                item_text = item_text
                    .saturating_add(
                        file.get("filePath")
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>")
                            .len(),
                    )
                    .saturating_add(128);
                if include_binary {
                    item_binary = file["base64"].as_str().unwrap().len();
                }
            }
            (Some("parts"), Some(file)) => {
                item_text = item_text
                    .saturating_add(
                        file.get("filePath")
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>")
                            .len(),
                    )
                    .saturating_add(128);
                if include_binary {
                    if let Some(pages) = file.get("pages").and_then(Value::as_array) {
                        for page in pages {
                            item_binary = item_binary.saturating_add(
                                page.get("base64")
                                    .and_then(Value::as_str)
                                    .map_or(0, str::len),
                            );
                        }
                    }
                }
            }
            _ => {
                let size = result.to_string().len();
                item_text = item_text.saturating_add(size.min(READ_BATCH_TEXT_BUDGET));
                if size > READ_BATCH_TEXT_BUDGET {
                    item_text = item_text.saturating_add(128);
                }
            }
        }
        if text.saturating_add(item_text) > READ_BATCH_TEXT_BUDGET - READ_BATCH_NOTICE_BUDGET
            || binary.saturating_add(item_binary) > READ_BATCH_BINARY_BUDGET
        {
            let partial = delivery.is_empty() && item_binary == 0;
            delivery.push(if partial {
                ReadBatchDelivery::Partial
            } else {
                ReadBatchDelivery::Omitted
            });
            stopped = true;
        } else {
            text += item_text;
            binary += item_binary;
            delivery.push(ReadBatchDelivery::Full);
        }
    }
    delivery
}
const DESCRIPTION: &str = "Reads files from the local filesystem. Put all files and ranges needed for this step into one call. Provide a non-empty `targets` array of at most 16 items; each target requires an absolute `path` and may specify inclusive 1-indexed `start` and `end`, a `symbol`, or PDF `pages`. If a symbol and range are both given, both are read.\n\
Text files of at most 600 lines are selected in full; larger files use windows of at least 300 lines, expanded to symbol boundaries where available for Rust, Python, JavaScript and TypeScript. Adjacent or overlapping windows in one call are merged. Each rendered text result has a 16 KiB output budget and lines over 4 KiB are clipped. Batch text output is limited to 32 KiB; omitted results are identified for retry. Text files over 16 MiB require search instead.\n\
Example: targets=[{path:\"/work/view.rs\"},{path:\"/work/draw.rs\",symbol:\"draw_node\"},{path:\"/work/style.rs\",start:1,end:200}].\n\
Images are returned as visual payloads. PDFs over 10 pages require `pages`; at most 20 pages per target. Files only, not directories.";
/// Default lines to read when the model does not provide an explicit
/// `limit`. Kept low (200) so that a single Read does not dump tens
/// of thousands of tokens into the context window. The model should
/// use `offset`/`limit` for targeted reads of larger files.
const DEFAULT_READ_LINES: usize = 200;
/// Hard cap on lines per read — applies even when an explicit `limit`
/// is provided. Prevents a single Read call from filling the context.
const MAX_LINES_TO_READ: usize = 2000;
/// Pre-read file size gate. Files larger than this (when no explicit
/// `limit` is provided) are rejected with a guiding error message.
const MAX_FILE_SIZE_BYTES: u64 = 256 * 1024; // 256 KB
const MAX_IMAGE_FILE_SIZE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_BATCH_TEXT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_GIF_DECODED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GIF_FRAMES: usize = 200;
/// Hard byte budget for a single Read's rendered output. Caps the
/// post-render UTF-8 byte length so a `limit` over a file with very
/// long lines (minified / obfuscated / generated) cannot blow up the
/// context window. Excess lines are dropped and reported via the
/// `truncation` note instead of erroring — the model can re-read a
/// narrower range.
const READ_MAX_OUTPUT_BYTES: usize = 16 * 1024;
/// Per-line byte cap. Lines longer than this get clipped to a head /
/// tail pair with a marker indicating how many bytes were omitted, so
/// a single 64 KB line can't consume the whole output budget by itself.
const READ_MAX_LINE_BYTES: usize = 4 * 1024;
/// Bytes kept from the head of an over-long line during clipping.
const READ_LINE_HEAD_BYTES: usize = 1024;
/// Bytes kept from the tail of an over-long line during clipping.
const READ_LINE_TAIL_BYTES: usize = 1024;
const INVALID_INPUT_CODE: i64 = 400;
const NOT_FOUND_CODE: i64 = 1;
const NOT_FILE_CODE: i64 = 2;
const UNSUPPORTED_MEDIA_CODE: i64 = 3;
const FILE_TOO_LARGE_CODE: i64 = 10;
const INVALID_PAGES_CODE: i64 = 7;
const PAGE_RANGE_TOO_LARGE_CODE: i64 = 8;
const PDF_TARGET_RAW_SIZE: u64 = 20 * 1024 * 1024;
const PDF_MAX_EXTRACT_SIZE: u64 = 100 * 1024 * 1024;
const PDF_MAX_PAGES_PER_READ: u32 = 20;
const PDF_INLINE_PAGE_THRESHOLD: u32 = 10;
const PDF_RENDER_DPI: &str = "100";
const PDFTOPPM_TIMEOUT: Duration = Duration::from_secs(120);
const PDFINFO_TIMEOUT: Duration = Duration::from_secs(10);
static PDF_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const SUPPORTED_IMAGE_MEDIA_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

#[derive(Debug, Clone, Default)]
pub struct ReadTool;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadInput {
    file_path: PathBuf,
    offset: usize,
    limit: Option<usize>,
    pages: Option<String>,
    symbol: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PdfPageRange {
    first_page: u32,
    last_page: Option<u32>,
}

#[async_trait]
impl Tool for ReadTool {
    fn id(&self) -> ToolId {
        ToolId::new(FILE_READ_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["FileReadTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::FileRead
    }

    fn file_target_field(&self) -> Option<&'static str> {
        Some("file_path")
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "targets": {
                    "type": "array", "minItems": 1, "maxItems": MAX_READ_TARGETS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "start": {"type": "integer", "minimum": 1},
                            "end": {"type": "integer", "minimum": 1},
                            "symbol": {"type": "string", "minLength": 1},
                            "pages": {"type": "string"}
                        },
                        "required": ["path"], "additionalProperties": false
                    }
                }
            },
            "required": ["targets"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(prepare_requests(self.id(), input, context).map(|_| ()))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let mut requests = prepare_requests(self.id(), &input, context)?;
        let mut results = Vec::new();
        let mut observations = Vec::new();
        // Do not publish a snapshot until its result is guaranteed to fit
        // the same budget used by the model-facing projection.
        let staged_cache = rebon_tools_core::file_state::FileStateCache::new();
        let staged_context = context.clone().with_file_state_cache(staged_cache.clone());
        while !requests.is_empty() {
            let first = requests.remove(0);
            let path = first.file_path.clone();
            if is_pdf_path(&path) || image_media_type_for_path(&path).is_some() {
                let result = if first.symbol.is_some() {
                    Err(ToolError::InvalidInput {
                        tool: self.id(),
                        reason: "Cannot resolve a symbol in a non-text file".into(),
                        error_code: Some(INVALID_INPUT_CODE),
                    })
                } else {
                    self.read_one(first, context).await
                };
                results.push(result.unwrap_or_else(|e| json!({"type":"error", "filePath":normalize_path(&path), "error":e.to_string()})));
                continue;
            }
            let mut group = vec![first];
            let mut i = 0;
            while i < requests.len() {
                if requests[i].file_path == path {
                    group.push(requests.remove(i));
                } else {
                    i += 1;
                }
            }
            let _guard = crate::lock_file_for_read(&path).await;
            let content = fs::File::open(&path).and_then(|file| {
                let mut bytes = Vec::new();
                file.take(MAX_BATCH_TEXT_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_BATCH_TEXT_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Text file exceeds the 16 MiB Read limit; use search to inspect it",
                    ));
                }
                String::from_utf8(bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            });
            let content = match content {
                Ok(content) => content,
                Err(e) => {
                    for _ in group {
                        results.push(json!({"type":"error", "filePath":normalize_path(&path), "error":e.to_string()}));
                    }
                    continue;
                }
            };
            let symbols = crate::read_outline::outline(&path, &content);
            let total = content.lines().count();
            let mut resolved = Vec::new();
            for mut request in group {
                if let Some(name) = request.symbol.take() {
                    let matches: Vec<_> = symbols
                        .as_ref()
                        .map(|s| s.iter().filter(|s| s.name == name).collect())
                        .unwrap_or_default();
                    let error = if symbols.is_none() {
                        Some(format!("Cannot parse file to resolve symbol `{name}`"))
                    } else if matches.is_empty() {
                        Some(format!("Symbol `{name}` not found"))
                    } else if matches.len() > 1 {
                        Some(format!("Symbol `{name}` is ambiguous"))
                    } else {
                        None
                    };
                    if let Some(error) = error {
                        results.push(json!({"type":"error", "filePath":normalize_path(&path), "error":error}));
                        continue;
                    }
                    request.offset = matches[0].start;
                    request.limit = Some(matches[0].end - matches[0].start + 1);
                }
                let requested_start = request.offset;
                let requested_end = requested_start
                    .saturating_add(request.limit.unwrap_or(1))
                    .saturating_sub(1)
                    .min(total);
                if total <= 600 {
                    request.offset = 1;
                    request.limit = Some(total.max(1));
                } else {
                    let requested_length = request.limit.unwrap_or(300);
                    let length = requested_length.max(300);
                    let mut start = request
                        .offset
                        .saturating_sub((length - requested_length) / 2)
                        .max(1);
                    let mut end = start.saturating_add(length).saturating_sub(1).min(total);
                    if request.offset <= total
                        && end.saturating_sub(start).saturating_add(1) < length
                    {
                        start = end.saturating_sub(length - 1).max(1);
                    }
                    if let Some(symbols) = &symbols {
                        for boundary in [true, false] {
                            let point = if boundary { start } else { end };
                            if let Some(s) = symbols
                                .iter()
                                .filter(|s| {
                                    if boundary {
                                        s.start < point && point <= s.end
                                    } else {
                                        s.start <= point && point < s.end
                                    }
                                })
                                .min_by_key(|s| s.end - s.start)
                            {
                                if boundary {
                                    start = s.start;
                                } else {
                                    end = s.end;
                                }
                            }
                        }
                    }
                    request.offset = start;
                    request.limit = Some(end.saturating_sub(start).saturating_add(1));
                }
                if let Some(previous) = context
                    .file_state_cache()
                    .and_then(|cache| cache.get(&path))
                {
                    if previous.content == content && !previous.is_partial_view {
                        if let (Some(offset), Some(limit)) = (previous.offset, previous.limit) {
                            let old_start = offset as usize;
                            let old_end =
                                old_start.saturating_add(limit as usize).saturating_sub(1);
                            let new_start = request.offset;
                            let new_end = new_start
                                .saturating_add(request.limit.unwrap_or(0))
                                .saturating_sub(1);
                            if old_start <= new_end.saturating_add(1)
                                && new_start <= old_end.saturating_add(1)
                            {
                                let start = old_start.min(new_start);
                                let end = old_end.max(new_end);
                                let lines = end.saturating_sub(start).saturating_add(1);
                                let bytes: usize = content
                                    .lines()
                                    .skip(start - 1)
                                    .take(lines)
                                    .map(|line| line.len().saturating_add(8))
                                    .sum();
                                if lines <= MAX_LINES_TO_READ && bytes <= READ_MAX_OUTPUT_BYTES {
                                    request.offset = start;
                                    request.limit = Some(lines);
                                }
                            }
                        }
                    }
                }
                let selected: Vec<_> = content
                    .lines()
                    .skip(request.offset.saturating_sub(1))
                    .take(request.limit.unwrap_or(300))
                    .collect();
                let rendered = render_with_byte_budget(&selected, request.offset);
                let emitted_end = request
                    .offset
                    .saturating_add(rendered.emitted_lines)
                    .saturating_sub(1);
                if emitted_end < requested_end && request.offset < requested_start {
                    let end = request
                        .offset
                        .saturating_add(request.limit.unwrap_or(0))
                        .saturating_sub(1);
                    request.offset = requested_start;
                    request.limit = Some(end.saturating_sub(requested_start).saturating_add(1));
                }
                resolved.push(request);
            }
            for request in merge_batch_requests(resolved, &content) {
                let result = self
                    .read_locked(request, &staged_context, Some(&content), Some(&symbols))
                    .await;
                if result.is_ok() {
                    if let Some(state) = staged_cache.get(&path) {
                        observations.push((results.len(), path.clone(), state));
                    }
                }
                results.push(result.unwrap_or_else(|e| json!({"type":"error", "filePath":normalize_path(&path), "error":e.to_string()})));
            }
        }
        if let Some(cache) = context.file_state_cache() {
            let delivery = read_batch_delivery(&results, true);
            for (index, path, state) in observations {
                if delivery[index] == ReadBatchDelivery::Full {
                    cache.set(&path, state);
                }
            }
        }
        Ok(json!({"type": "batch", "results": results}))
    }
}

impl ReadTool {
    async fn read_one(&self, parsed: ReadInput, context: &ToolContext) -> ToolResult<Value> {
        if context.coordinator_mode() {
            enforce_coordinator_report_read_allowed(&parsed.file_path, context)?;
        }

        let _file_guard = crate::lock_file_for_read(&parsed.file_path).await;
        self.read_locked(parsed, context, None, None).await
    }

    async fn read_locked(
        &self,
        parsed: ReadInput,
        context: &ToolContext,
        cached: Option<&str>,
        batch_outline: Option<&Option<Vec<crate::read_outline::Symbol>>>,
    ) -> ToolResult<Value> {
        // Pre-read size gate: when no explicit limit is specified,
        // reject files larger than MAX_FILE_SIZE_BYTES. When a limit
        // IS specified the user is already reading a slice, so we
        // skip this gate and allow the requested slice to be read.
        let is_image = image_media_type_for_path(&parsed.file_path).is_some();
        let is_pdf = is_pdf_path(&parsed.file_path);
        if !is_pdf && cached.is_none() && (is_image || parsed.limit.is_none()) {
            let metadata = fs::metadata(&parsed.file_path).map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            })?;
            let file_size = metadata.len();
            let max_file_size = if is_image {
                MAX_IMAGE_FILE_SIZE_BYTES
            } else {
                MAX_FILE_SIZE_BYTES
            };
            if file_size > max_file_size {
                let reason = if is_image {
                    format!(
                        "Image file ({}) exceeds the maximum safe size ({}). Resize or recompress the image, then try again.",
                        format_file_size(file_size),
                        format_file_size(max_file_size),
                    )
                } else {
                    format!(
                        "File content ({}) exceeds maximum allowed size ({}). \
                         Use target `start` and `end` to read specific portions \
                         of the file, or search for specific content instead of \
                         reading the whole file.",
                        format_file_size(file_size),
                        format_file_size(max_file_size),
                    )
                };
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason,
                    error_code: Some(FILE_TOO_LARGE_CODE),
                });
            }
        }

        if is_pdf {
            return read_pdf_file(&parsed.file_path, parsed.pages.as_deref()).await;
        }

        let bytes = if let Some(text) = cached {
            text.as_bytes().to_vec()
        } else {
            fs::read(&parsed.file_path).map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            })?
        };

        if let Some(media_type) = image_media_type_for_path(&parsed.file_path) {
            validate_image_bytes(&parsed.file_path, media_type, &bytes)?;
            return Ok(json!({
                "type": "image",
                "file": {
                    "filePath": normalize_path(&parsed.file_path),
                    "base64": BASE64.encode(&bytes),
                    "type": media_type,
                }
            }));
        }

        let content = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!(
                        "Unsupported non-text file in current Rust slice: {}",
                        parsed.file_path.display()
                    ),
                    error_code: Some(UNSUPPORTED_MEDIA_CODE),
                })
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let start_line = parsed.offset;
        let zero_based_start = start_line.saturating_sub(1);
        // When the model provides an explicit `limit`, respect it
        // (capped to MAX_LINES_TO_READ). Otherwise use the lower
        // DEFAULT_READ_LINES so a single Read doesn't flood the
        // context window.
        let effective_limit = match parsed.limit {
            Some(l) => {
                if batch_outline.is_some() {
                    l
                } else {
                    l.min(MAX_LINES_TO_READ)
                }
            }
            None => DEFAULT_READ_LINES,
        };
        let available = total_lines.saturating_sub(zero_based_start);
        let was_line_truncated = parsed.limit.is_none() && available > effective_limit;
        let selected: Vec<&str> = lines
            .iter()
            .skip(zero_based_start)
            .take(effective_limit)
            .copied()
            .collect();
        let render = render_with_byte_budget(&selected, start_line);
        let rendered = render.content;
        let emitted_lines = render.emitted_lines;
        let was_byte_truncated = render.budget_hit;

        // When any form of truncation kicked in (line count, byte
        // budget, or per-line clipping), tell the model what it
        // missed so it can issue a targeted follow-up read.
        let truncation_note = build_truncation_note(TruncationContext {
            total_lines,
            start_line,
            requested_lines: selected.len(),
            emitted_lines,
            was_line_truncated,
            was_byte_truncated,
            clipped_lines: &render.clipped_lines,
            largest_line: render.largest_line,
        });

        let mut result = json!({
            "type": "text",
            "file": {
                "filePath": normalize_path(&parsed.file_path),
                "content": rendered,
                "numLines": emitted_lines,
                "startLine": start_line,
                "totalLines": total_lines,
            }
        });
        if let Some(batch_symbols) = batch_outline {
            if let Some(symbols) = batch_symbols {
                let last = start_line.saturating_add(emitted_lines).saturating_sub(1);
                let mut outline = String::new();
                let mut truncated = false;
                for symbol in symbols
                    .iter()
                    .filter(|s| emitted_lines == 0 || s.start < start_line || s.end > last)
                {
                    let line = format!(
                        "{} {} ({}-{})\n",
                        symbol.kind, symbol.name, symbol.start, symbol.end
                    );
                    if outline.len() + line.len() > 4096 {
                        truncated = true;
                        break;
                    }
                    outline.push_str(&line);
                }
                if truncated {
                    outline.push_str("[outline truncated]\n");
                }
                result["outline"] = json!(outline);
            } else {
                result["outline"] =
                    json!("Outline unavailable: unsupported language or parse error");
            }
        }
        if let Some(note) = truncation_note {
            result["truncation"] = json!(note);
        }

        // Register this observation in the per-session file-state
        // cache so that Edit / Write can enforce "must Read before
        // edit" and detect external mods. Store the RAW disk content,
        // not the rendered line-numbered text — Edit's post-write
        // content comparison compares against what's on disk.
        //
        // `is_partial_view` is intentionally **always false** on the
        // Read path. The flag is a semantic marker for auto-injected
        // content (REBON.md / MEMORY.md) whose processed form no
        // longer matches disk bytes — not for user-driven `offset` /
        // `limit` / auto-truncation; it is set only when the injected
        // content differs from the bytes on disk.
        if let Some(cache) = context.file_state_cache() {
            let had_explicit_range = start_line != 1
                || was_line_truncated
                || was_byte_truncated
                || emitted_lines < total_lines;
            let timestamp_ms = file_mtime_ms(&parsed.file_path).unwrap_or(0);
            cache.set(
                &parsed.file_path,
                FileState {
                    content: content.clone(),
                    timestamp_ms,
                    offset: if had_explicit_range {
                        Some(start_line as u64)
                    } else {
                        None
                    },
                    limit: if had_explicit_range {
                        Some(emitted_lines as u64)
                    } else {
                        None
                    },
                    is_partial_view: false,
                },
            );
        }

        Ok(result)
    }
}

fn prepare_requests(
    tool: ToolId,
    input: &Value,
    context: &ToolContext,
) -> ToolResult<Vec<ReadInput>> {
    let invalid = |reason: &str| ToolError::InvalidInput {
        tool: ToolId::new(FILE_READ_TOOL_NAME),
        reason: reason.into(),
        error_code: Some(INVALID_INPUT_CODE),
    };
    let object = input
        .as_object()
        .ok_or_else(|| invalid("Read input must be an object"))?;
    if object.keys().any(|key| key != "targets") {
        return Err(invalid("Only `targets` is allowed in Read input"));
    }
    let targets = object
        .get("targets")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| invalid("`targets` must be a non-empty array"))?;
    if targets.len() > MAX_READ_TARGETS {
        return Err(invalid("`targets` may contain at most 16 items"));
    }
    let mut requests = Vec::with_capacity(targets.len());
    for target in targets {
        let obj = target
            .as_object()
            .ok_or_else(|| invalid("Each target must be an object"))?;
        if obj
            .keys()
            .any(|key| !matches!(key.as_str(), "path" | "start" | "end" | "pages" | "symbol"))
        {
            return Err(invalid("Unknown target field"));
        }
        let symbol = match obj.get("symbol").filter(|value| !value.is_null()) {
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("Target `symbol` must be a non-empty string"))?
                    .to_owned(),
            ),
            None => None,
        };
        let start = match obj.get("start").filter(|value| !value.is_null()) {
            Some(value) => value
                .as_u64()
                .filter(|n| *n > 0)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| invalid("Target `start` must be a positive integer"))?,
            None => 1,
        };
        let end = match obj.get("end").filter(|value| !value.is_null()) {
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|n| *n > 0)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| invalid("Target `end` must be a positive integer"))?,
            ),
            None => None,
        };
        if end.is_some_and(|end| end < start) {
            return Err(invalid("Target `end` must not precede `start`"));
        }
        let mut single = json!({"file_path": obj.get("path"), "offset": start});
        if let Some(end) = end {
            single["limit"] = json!(end.saturating_sub(start).saturating_add(1));
        }
        if let Some(pages) = obj.get("pages").filter(|value| !value.is_null()) {
            single["pages"] = pages.clone();
        }
        let mut parsed = prepare_input(tool.clone(), &single, context)?;
        if context.coordinator_mode() {
            enforce_coordinator_report_read_allowed(&parsed.file_path, context)?;
        }
        if symbol.is_some() && (obj.get("start").is_some_and(|v| !v.is_null()) || end.is_some()) {
            requests.push(parsed.clone());
        }
        parsed.symbol = symbol;
        requests.push(parsed);
    }
    Ok(requests)
}

// Batch ranges are expanded before coalescing, so adjacent expanded windows
// are read once. Never merge past the existing per-read hard line cap.
fn merge_batch_requests(requests: Vec<ReadInput>, content: &str) -> Vec<ReadInput> {
    let mut merged: Vec<ReadInput> = Vec::new();
    for request in requests {
        if is_pdf_path(&request.file_path)
            || image_media_type_for_path(&request.file_path).is_some()
        {
            merged.push(request);
            continue;
        }
        let mut current = request;
        let mut i = 0;
        while i < merged.len() {
            let other = &merged[i];
            let start = current.offset.min(other.offset);
            let end = current
                .offset
                .saturating_add(current.limit.unwrap_or(300))
                .saturating_sub(1);
            let other_end = other
                .offset
                .saturating_add(other.limit.unwrap_or(300))
                .saturating_sub(1);
            let union_end = end.max(other_end);
            if other.file_path == current.file_path
                && other.pages == current.pages
                && current.offset <= other_end.saturating_add(1)
                && other.offset <= end.saturating_add(1)
                && union_end.saturating_sub(start) < MAX_LINES_TO_READ
                && !render_with_byte_budget(
                    &content
                        .lines()
                        .skip(start.saturating_sub(1))
                        .take(union_end - start + 1)
                        .collect::<Vec<_>>(),
                    start,
                )
                .budget_hit
            {
                current.offset = start;
                current.limit = Some(union_end - start + 1);
                merged.remove(i);
                i = 0;
            } else {
                i += 1;
            }
        }
        merged.push(current);
    }
    merged
}

/// Parse and vet one `Read` request, once.
///
/// The tool's two entry points ask the same three questions in the same
/// order — does the input parse, does the path clear the read policy, does
/// the parsed request make sense — so they ask them here. `validate_input`
/// reports the answer as a verdict, `call` takes the parsed request and runs.
fn prepare_input(tool: ToolId, input: &Value, context: &ToolContext) -> ToolResult<ReadInput> {
    let parsed = parse_input(input)?;
    crate::path_scope::enforce_read_path_policy(tool.clone(), context, &parsed.file_path, "path")?;
    require_valid_input(
        tool,
        validate_parsed_input(&parsed)?,
        "Read input is invalid",
    )?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<ReadInput> {
    let tool = ToolId::new(FILE_READ_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Read input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let file_path = object
        .get("file_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "Read target requires a non-empty `path` string".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

    let offset = match object.get("offset") {
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "`offset` must be an integer >= 0".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })? as usize,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`offset` must be an integer when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
        None => 1,
    };

    let limit = match object.get("limit") {
        Some(Value::Number(n)) => {
            Some(
                n.as_u64()
                    .filter(|v| *v >= 1)
                    .ok_or_else(|| ToolError::InvalidInput {
                        tool: tool.clone(),
                        reason: "`limit` must be an integer >= 1".into(),
                        error_code: Some(INVALID_INPUT_CODE),
                    })? as usize,
            )
        }
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`limit` must be an integer when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
        None => None,
    };

    let pages = match object.get("pages") {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        Some(Value::String(_)) | Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool,
                reason: "`pages` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    Ok(ReadInput {
        file_path: PathBuf::from(file_path),
        offset: offset.max(1),
        limit,
        symbol: None,
        pages,
    })
}

fn validate_parsed_input(input: &ReadInput) -> ToolResult<ValidationOutcome> {
    if !input.file_path.is_absolute() {
        return Ok(ValidationOutcome::invalid(
            format!(
                "Read requires an absolute `path`, got: {}",
                input.file_path.display()
            ),
            INVALID_INPUT_CODE,
        ));
    }

    if let Some(pages) = input.pages.as_deref() {
        let Some(range) = parse_pdf_page_range(pages) else {
            return Ok(ValidationOutcome::invalid(
                format!(
                    "Invalid pages parameter: \"{pages}\". Use formats like \"1-5\", \"3\", \"10-\", or \"10-20\". Pages are 1-indexed."
                ),
                INVALID_PAGES_CODE,
            ));
        };
        let range_size = range
            .last_page
            .map(|last_page| last_page - range.first_page + 1)
            .unwrap_or(PDF_MAX_PAGES_PER_READ);
        if range_size > PDF_MAX_PAGES_PER_READ {
            return Ok(ValidationOutcome::invalid(
                format!(
                    "Page range \"{pages}\" exceeds maximum of {PDF_MAX_PAGES_PER_READ} pages per request. Please use a smaller range."
                ),
                PAGE_RANGE_TOO_LARGE_CODE,
            ));
        }
    }

    match fs::metadata(&input.file_path) {
        Ok(metadata) if !metadata.is_file() => Ok(ValidationOutcome::invalid(
            format!("Path is not a file: {}", input.file_path.display()),
            NOT_FILE_CODE,
        )),
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ValidationOutcome::invalid(
            format!("File does not exist: {}", input.file_path.display()),
            NOT_FOUND_CODE,
        )),
        Err(err) => Err(ToolError::Execution {
            tool: ToolId::new(FILE_READ_TOOL_NAME),
            source: err.into(),
        }),
    }
}

fn coordinator_read_path_matches(requested: &Path, allowed: &Path) -> bool {
    let requested_canonical = fs::canonicalize(requested).ok();
    let allowed_canonical = fs::canonicalize(allowed).ok();
    match (requested_canonical, allowed_canonical) {
        (Some(requested), Some(allowed)) => requested == allowed,
        _ => requested == allowed,
    }
}

fn enforce_coordinator_report_read_allowed(path: &Path, context: &ToolContext) -> ToolResult<()> {
    let allowed = context.coordinator_report_paths();
    if allowed
        .iter()
        .any(|allowed_path| coordinator_read_path_matches(path, allowed_path))
    {
        return Ok(());
    }

    Err(ToolError::InvalidInput {
        tool: ToolId::new(FILE_READ_TOOL_NAME),
        reason: format!(
            "Coordinator mode may only use Read on registered worker report files; `{}` is not in the current report allowlist.",
            path.display()
        ),
        error_code: Some(INVALID_INPUT_CODE),
    })
}

fn image_media_type_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    SUPPORTED_IMAGE_MEDIA_TYPES
        .iter()
        .find_map(|(candidate, media_type)| (*candidate == ext).then_some(*media_type))
}

fn validate_image_bytes(path: &Path, media_type: &str, bytes: &[u8]) -> ToolResult<()> {
    let format = match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => unreachable!("unsupported image media type: {media_type}"),
    };

    let valid = if format == ImageFormat::Gif {
        validate_gif_frames(bytes)
    } else {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(image_decode_limits());
        reader.decode().map(|_| ()).map_err(|_| ())
    };

    valid.map_err(|_| ToolError::InvalidInput {
        tool: ToolId::new(FILE_READ_TOOL_NAME),
        reason: format!(
            "Cannot read `{}` as an image: its contents are not valid {} data or exceed safe decoding limits. The file may be incomplete or mislabeled; regenerate, resize, or correct its extension, then try again.",
            path.display(),
            media_type
        ),
        error_code: Some(INVALID_INPUT_CODE),
    })
}

fn image_decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    limits
}

fn validate_gif_frames(bytes: &[u8]) -> Result<(), ()> {
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|_| ())?;
    decoder.set_limits(image_decode_limits()).map_err(|_| ())?;
    let mut frames = decoder.into_frames();
    let mut frame_count = 0;
    let mut decoded_bytes = 0_u64;

    while let Some(frame) = frames.next() {
        let frame = frame.map_err(|_| ())?;
        frame_count += 1;
        decoded_bytes = decoded_bytes
            .checked_add(frame.buffer().len() as u64)
            .ok_or(())?;
        if frame_count > MAX_GIF_FRAMES || decoded_bytes > MAX_GIF_DECODED_BYTES {
            return Err(());
        }
    }

    (frame_count > 0).then_some(()).ok_or(())
}

#[derive(Debug, Default)]
struct RenderResult {
    /// Rendered, line-numbered text — guaranteed valid UTF-8 and no
    /// larger than `READ_MAX_OUTPUT_BYTES` plus the size of one final
    /// line (we admit the line first and only check the budget when
    /// deciding whether to emit the *next* line).
    content: String,
    /// Number of source lines successfully emitted into `content`.
    /// Always less than or equal to `selected.len()`.
    emitted_lines: usize,
    /// True when the byte budget stopped us from emitting more lines.
    /// Not the same as per-line clipping (`clipped_lines`).
    budget_hit: bool,
    /// `(line_number, bytes_omitted)` for every line we shortened with
    /// the per-line clip path.
    clipped_lines: Vec<(usize, usize)>,
    /// `(line_number, raw_byte_length)` of the longest input line we
    /// observed in the selected range. `None` when the range is empty.
    largest_line: Option<(usize, usize)>,
}

fn render_with_byte_budget(selected: &[&str], start_line: usize) -> RenderResult {
    let mut result = RenderResult::default();
    for (idx, line) in selected.iter().enumerate() {
        let line_number = start_line + idx;
        let line_bytes = line.len();
        match result.largest_line {
            Some((_, prev)) if line_bytes <= prev => {}
            _ => result.largest_line = Some((line_number, line_bytes)),
        }

        let (rendered_line, omitted_bytes) = if line_bytes > READ_MAX_LINE_BYTES {
            clip_long_line(line)
        } else {
            (line.to_string(), 0)
        };
        if omitted_bytes > 0 {
            result.clipped_lines.push((line_number, omitted_bytes));
        }

        // Per-entry overhead: 6-char right-aligned line number + tab,
        // plus the leading newline once we've emitted at least one
        // entry. Computed against the *clipped* line so the budget
        // check uses what we'd actually write.
        let separator_bytes = if result.emitted_lines == 0 { 0 } else { 1 };
        let entry_bytes = separator_bytes + 7 + rendered_line.len();
        if result.emitted_lines > 0 && result.content.len() + entry_bytes > READ_MAX_OUTPUT_BYTES {
            result.budget_hit = true;
            break;
        }
        if result.emitted_lines > 0 {
            result.content.push('\n');
        }
        result
            .content
            .push_str(&format!("{:>6}\t{}", line_number, rendered_line));
        result.emitted_lines += 1;
    }
    result
}

/// Clip a single over-long line to a head / tail pair joined by a
/// human-readable marker recording how many bytes we dropped. Both
/// cut points snap to UTF-8 char boundaries so we never produce a
/// half-character.
fn clip_long_line(line: &str) -> (String, usize) {
    let len = line.len();
    let head_end = floor_char_boundary(line, READ_LINE_HEAD_BYTES);
    let tail_start = ceil_char_boundary(line, len.saturating_sub(READ_LINE_TAIL_BYTES));
    if head_end >= tail_start {
        return (line.to_string(), 0);
    }
    let omitted_bytes = tail_start - head_end;
    let head = &line[..head_end];
    let tail = &line[tail_start..];
    (
        format!("{head} ... [line clipped: {omitted_bytes} bytes omitted] ... {tail}"),
        omitted_bytes,
    )
}

fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(s: &str, mut index: usize) -> usize {
    let len = s.len();
    if index >= len {
        return len;
    }
    while index < len && !s.is_char_boundary(index) {
        index += 1;
    }
    index
}

struct TruncationContext<'a> {
    total_lines: usize,
    start_line: usize,
    requested_lines: usize,
    emitted_lines: usize,
    was_line_truncated: bool,
    was_byte_truncated: bool,
    clipped_lines: &'a [(usize, usize)],
    largest_line: Option<(usize, usize)>,
}

fn build_truncation_note(ctx: TruncationContext<'_>) -> Option<String> {
    if !ctx.was_line_truncated && !ctx.was_byte_truncated && ctx.clipped_lines.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let last_emitted = ctx
        .start_line
        .saturating_add(ctx.emitted_lines.saturating_sub(1));
    parts.push(format!(
        "File has {total} total lines. Returned lines {start}-{end} ({emitted} lines).",
        total = ctx.total_lines,
        start = ctx.start_line,
        end = last_emitted,
        emitted = ctx.emitted_lines,
    ));

    if ctx.was_byte_truncated {
        let next_line = last_emitted.saturating_add(1);
        let last_requested = ctx
            .start_line
            .saturating_add(ctx.requested_lines.saturating_sub(1));
        let omitted_lines = ctx.requested_lines.saturating_sub(ctx.emitted_lines);
        parts.push(format!(
            "Output truncated: exceeded {budget} KiB byte budget. Omitted requested lines \
             {next_line}-{last_requested} ({omitted_lines} lines).",
            budget = READ_MAX_OUTPUT_BYTES / 1024,
        ));
        if let Some((line_no, bytes)) = ctx.largest_line {
            parts.push(format!(
                "Largest line seen: line {line_no} ({bytes} bytes)."
            ));
        }
        parts.push(format!(
            "Hint: retry with target `start` at {next_line}; windows may be expanded. \
             Use Grep/Bash to extract content from very long lines."
        ));
    } else if ctx.was_line_truncated {
        parts.push("Use target `start` and `end` to read more.".into());
    }

    if !ctx.clipped_lines.is_empty() {
        let sample: Vec<String> = ctx
            .clipped_lines
            .iter()
            .take(3)
            .map(|(line_no, bytes)| format!("line {line_no} ({bytes} bytes omitted)"))
            .collect();
        let more = if ctx.clipped_lines.len() > sample.len() {
            format!(" (+{} more)", ctx.clipped_lines.len() - sample.len())
        } else {
            String::new()
        };
        parts.push(format!(
            "Long lines clipped (max {limit} bytes/line): {sample}{more}.",
            limit = READ_MAX_LINE_BYTES,
            sample = sample.join(", "),
        ));
    }

    Some(parts.join(" "))
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn format_file_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn parse_pdf_page_range(pages: &str) -> Option<PdfPageRange> {
    let trimmed = pages.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(first) = trimmed.strip_suffix('-') {
        let first_page = first.parse::<u32>().ok()?;
        if first_page == 0 {
            return None;
        }
        return Some(PdfPageRange {
            first_page,
            last_page: None,
        });
    }

    if let Some((first, last)) = trimmed.split_once('-') {
        let first_page = first.parse::<u32>().ok()?;
        let last_page = last.parse::<u32>().ok()?;
        if first_page == 0 || last_page == 0 || last_page < first_page {
            return None;
        }
        return Some(PdfPageRange {
            first_page,
            last_page: Some(last_page),
        });
    }

    let page = trimmed.parse::<u32>().ok()?;
    if page == 0 {
        return None;
    }
    Some(PdfPageRange {
        first_page: page,
        last_page: Some(page),
    })
}

async fn read_pdf_file(path: &Path, pages: Option<&str>) -> ToolResult<Value> {
    let tool = ToolId::new(FILE_READ_TOOL_NAME);
    if let Some(pages) = pages {
        let range = parse_pdf_page_range(pages).ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!(
                "Invalid pages parameter: \"{pages}\". Use formats like \"1-5\", \"3\", \"10-\", or \"10-20\". Pages are 1-indexed."
            ),
            error_code: Some(INVALID_PAGES_CODE),
        })?;
        return extract_pdf_pages(path, range).await;
    }

    if let Some(page_count) = get_pdf_page_count(path).await {
        if page_count > PDF_INLINE_PAGE_THRESHOLD {
            return Err(ToolError::InvalidInput {
                tool,
                reason: format!(
                    "This PDF has {page_count} pages, which is too many to read at once. \
                     Use the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). \
                     Maximum {PDF_MAX_PAGES_PER_READ} pages per request."
                ),
                error_code: Some(PAGE_RANGE_TOO_LARGE_CODE),
            });
        }
    }

    let metadata = fs::metadata(path).map_err(|err| ToolError::Execution {
        tool: tool.clone(),
        source: err.into(),
    })?;
    let original_size = metadata.len();
    validate_pdf_file(path, original_size, PDF_TARGET_RAW_SIZE)?;

    let bytes = fs::read(path).map_err(|err| ToolError::Execution {
        tool: tool.clone(),
        source: err.into(),
    })?;

    Ok(json!({
        "type": "pdf",
        "file": {
            "filePath": normalize_path(path),
            "base64": BASE64.encode(&bytes),
            "originalSize": original_size,
        }
    }))
}

fn validate_pdf_file(path: &Path, original_size: u64, max_size: u64) -> ToolResult<()> {
    let tool = ToolId::new(FILE_READ_TOOL_NAME);
    if original_size == 0 {
        return Err(ToolError::InvalidInput {
            tool,
            reason: format!("PDF file is empty: {}", path.display()),
            error_code: Some(UNSUPPORTED_MEDIA_CODE),
        });
    }
    if original_size > max_size {
        return Err(ToolError::InvalidInput {
            tool,
            reason: format!(
                "PDF file exceeds maximum allowed size of {}.",
                format_file_size(max_size)
            ),
            error_code: Some(FILE_TOO_LARGE_CODE),
        });
    }

    let mut file = fs::File::open(path).map_err(|err| ToolError::Execution {
        tool: ToolId::new(FILE_READ_TOOL_NAME),
        source: err.into(),
    })?;
    let mut header = [0u8; 5];
    file.read_exact(&mut header)
        .map_err(|err| ToolError::Execution {
            tool: ToolId::new(FILE_READ_TOOL_NAME),
            source: err.into(),
        })?;
    if header != *b"%PDF-" {
        return Err(ToolError::InvalidInput {
            tool: ToolId::new(FILE_READ_TOOL_NAME),
            reason: format!(
                "File is not a valid PDF (missing %PDF- header): {}",
                path.display()
            ),
            error_code: Some(UNSUPPORTED_MEDIA_CODE),
        });
    }

    Ok(())
}

async fn get_pdf_page_count(path: &Path) -> Option<u32> {
    let output = tokio::time::timeout(
        PDFINFO_TIMEOUT,
        Command::new("pdfinfo")
            .arg(path)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        let value = line.strip_prefix("Pages:")?.trim();
        value.parse::<u32>().ok()
    })
}

async fn is_pdftoppm_available() -> bool {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("pdftoppm")
            .arg("-v")
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) => output.status.success() || !output.stderr.is_empty(),
        _ => false,
    }
}

async fn extract_pdf_pages(path: &Path, range: PdfPageRange) -> ToolResult<Value> {
    let tool = ToolId::new(FILE_READ_TOOL_NAME);
    let metadata = fs::metadata(path).map_err(|err| ToolError::Execution {
        tool: tool.clone(),
        source: err.into(),
    })?;
    let original_size = metadata.len();
    validate_pdf_file(path, original_size, PDF_MAX_EXTRACT_SIZE)?;
    if !is_pdftoppm_available().await {
        return Err(ToolError::InvalidInput {
            tool,
            reason: "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering.".into(),
            error_code: Some(UNSUPPORTED_MEDIA_CODE),
        });
    }

    let output_dir = pdf_output_dir();
    fs::create_dir_all(&output_dir).map_err(|err| ToolError::Execution {
        tool: tool.clone(),
        source: err.into(),
    })?;
    let prefix = output_dir.join("page");
    let mut args = vec![
        "-jpeg".to_string(),
        "-r".to_string(),
        PDF_RENDER_DPI.to_string(),
    ];
    args.push("-f".to_string());
    args.push(range.first_page.to_string());
    if let Some(last_page) = range.last_page {
        args.push("-l".to_string());
        args.push(last_page.to_string());
    }
    args.push(path.to_string_lossy().to_string());
    args.push(prefix.to_string_lossy().to_string());

    let output = tokio::time::timeout(
        PDFTOPPM_TIMEOUT,
        Command::new("pdftoppm")
            .args(&args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "pdftoppm timed out while rendering PDF pages.".into(),
        error_code: Some(UNSUPPORTED_MEDIA_CODE),
    })?
    .map_err(|err| ToolError::Execution {
        tool: tool.clone(),
        source: err.into(),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_ascii_lowercase();
        let reason = if lower.contains("password") {
            "PDF is password-protected. Please provide an unprotected version.".to_string()
        } else if lower.contains("damaged")
            || lower.contains("corrupt")
            || lower.contains("invalid")
        {
            "PDF file is corrupted or invalid.".to_string()
        } else {
            format!("pdftoppm failed: {stderr}")
        };
        return Err(ToolError::InvalidInput {
            tool,
            reason,
            error_code: Some(UNSUPPORTED_MEDIA_CODE),
        });
    }

    let mut image_files = fs::read_dir(&output_dir)
        .map_err(|err| ToolError::Execution {
            tool: tool.clone(),
            source: err.into(),
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
        })
        .collect::<Vec<_>>();
    image_files.sort_by(|a, b| pdf_page_image_sort_key(a).cmp(&pdf_page_image_sort_key(b)));

    if image_files.is_empty() {
        return Err(ToolError::InvalidInput {
            tool,
            reason: "pdftoppm produced no output pages. The PDF may be invalid.".into(),
            error_code: Some(UNSUPPORTED_MEDIA_CODE),
        });
    }

    let mut page_images = Vec::with_capacity(image_files.len());
    for image_path in &image_files {
        let bytes = fs::read(image_path).map_err(|err| ToolError::Execution {
            tool: tool.clone(),
            source: err.into(),
        })?;
        page_images.push(json!({
            "filePath": normalize_path(image_path),
            "base64": BASE64.encode(&bytes),
            "type": "image/jpeg",
        }));
    }

    Ok(json!({
        "type": "parts",
        "file": {
            "filePath": normalize_path(path),
            "originalSize": original_size,
            "count": image_files.len(),
            "outputDir": normalize_path(&output_dir),
            "pages": page_images,
        }
    }))
}

fn pdf_page_image_sort_key(path: &Path) -> (u32, String) {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let page = name
        .rsplit_once('-')
        .and_then(|(_, suffix)| suffix.parse::<u32>().ok())
        .unwrap_or(u32::MAX);
    (page, name.to_string())
}

fn pdf_output_dir() -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = PDF_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rebon-pdf-pages-{}-{millis}-{counter}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_tools_core::file_state::FileStateCache;
    use serde_json::json;

    #[test]
    fn description_encourages_batching_files_and_ranges() {
        assert!(ReadTool
            .description()
            .contains("Put all files and ranges needed for this step into one call"));
    }

    struct TempDir {
        inner: tempfile::TempDir,
    }

    impl TempDir {
        fn new() -> Self {
            Self {
                inner: tempfile::Builder::new()
                    .prefix("rebon-read-tool-test-")
                    .tempdir()
                    .unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }
    }

    fn tool() -> ReadTool {
        ReadTool
    }

    #[tokio::test]
    async fn batch_target_count_is_bounded_in_schema_and_execution() {
        let dir = TempDir::new();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello\n").unwrap();
        let context = ToolContext::new();
        let target = json!({"path": path});
        let accepted = json!({"targets": vec![target.clone(); 16]});
        let rejected = json!({"targets": vec![target; 17]});
        assert_eq!(
            tool().input_schema()["properties"]["targets"]["maxItems"],
            16
        );
        assert!(tool()
            .validate_input(&accepted, &context)
            .await
            .unwrap()
            .is_valid());
        assert!(!tool()
            .validate_input(&rejected, &context)
            .await
            .unwrap()
            .is_valid());
        assert!(tool().call(rejected, &context).await.is_err());
    }

    #[tokio::test]
    async fn rejects_legacy_and_unknown_input_fields() {
        let dir = TempDir::new();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello\n").unwrap();
        let context = ToolContext::new();
        for input in [
            json!({"file_path": path}),
            json!({"targets": [{"path": path}], "offset": 1}),
            json!({"targets": [{"path": path, "limit": 1}]}),
            json!({"targets": []}),
        ] {
            assert!(!tool()
                .validate_input(&input, &context)
                .await
                .unwrap()
                .is_valid());
            assert!(tool().call(input, &context).await.is_err());
        }
        let schema = tool().input_schema();
        assert_eq!(schema["required"], json!(["targets"]));
        assert!(schema["properties"].get("file_path").is_none());
        assert!(schema["properties"]["targets"]["items"]["properties"]
            .get("symbol")
            .is_some());
    }

    #[tokio::test]
    async fn batch_accepts_null_optional_fields_and_symbol_with_range() {
        let dir = TempDir::new();
        let path = dir.path().join("source.rs");
        fs::write(
            &path,
            format!(
                "{}fn target() {{}}\n{}",
                "// before\n".repeat(400),
                "// after\n".repeat(400)
            ),
        )
        .unwrap();
        let context = ToolContext::new();
        let output = tool()
            .call(
                json!({"targets":[
                    {"path":path, "symbol":null, "start":null, "end":null, "pages":null},
                    {"path":path, "symbol":"target", "start":null, "end":null},
                    {"path":path, "symbol":"target", "start":350, "end":450}
                ]}),
                &context,
            )
            .await
            .unwrap();
        assert!(
            output["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r.get("error").is_none()),
            "{output}"
        );
        let invalid = tool()
            .call(
                json!({"targets":[{"path":path,"symbol":"target","start":0}]}),
                &context,
            )
            .await;
        assert!(invalid.is_err());

        fs::write(
            &path,
            format!(
                "{}fn target() {{}}\n{}",
                "// before\n".repeat(800),
                "// after\n".repeat(800)
            ),
        )
        .unwrap();
        let output = tool()
            .call(
                json!({"targets":[{"path":path,"symbol":"target","start":1,"end":1}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let results = output["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "{output}");
        assert!(results.iter().any(|r| r["file"]["startLine"] == 1));
        assert!(results.iter().any(|r| r["file"]["content"]
            .as_str()
            .is_some_and(|s| s.contains("fn target()"))));
    }

    #[tokio::test]
    async fn batch_symbols_boundaries_and_errors() {
        let dir = TempDir::new();
        let path = dir.path().join("symbols.rs");
        let source = format!(
            "{}impl Thing {{\n    fn wanted() {{\n{}    }}\n}}\n{}fn hidden() {{}}\n",
            "// padding\n".repeat(300),
            "        // body\n".repeat(400),
            "// padding\n".repeat(200)
        );
        fs::write(&path, source).unwrap();
        let result = tool()
            .call(
                json!({"targets":[{"path":path,"start":500,"end":501}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let item = &result["results"][0];
        assert_eq!(item["file"]["startLine"], 302);
        assert!(item["outline"].as_str().unwrap().contains("hidden"));
        let result = tool().call(json!({"targets":[{"path":path,"symbol":"wanted"},{"path":path,"symbol":"missing"}]}), &ToolContext::new()).await.unwrap();
        assert!(result["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["error"].as_str().unwrap_or("").contains("not found")));
        let result = tool()
            .call(
                json!({"targets":[{"path":path,"symbol":"wanted","start":1}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(result["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.get("error").is_none()));
        fs::write(&path, "fn duplicate() {}\nfn duplicate() {}\n").unwrap();
        let result = tool()
            .call(
                json!({"targets":[{"path":path,"symbol":"duplicate"}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(result["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("ambiguous"));
        fs::write(&path, "fn broken( {").unwrap();
        let result = tool()
            .call(
                json!({"targets":[{"path":path,"symbol":"broken"}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(result["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("Cannot parse"));
    }

    #[tokio::test]
    async fn explicit_targets_remain_reachable_with_output_budget() {
        let dir = TempDir::new();
        let small = dir.path().join("small.rs");
        let large = dir.path().join("large.txt");
        fs::write(
            &small,
            format!(
                "{}fn tail_target() {{}}\n",
                format!("{}\n", "//0123456789".repeat(8)).repeat(499)
            ),
        )
        .unwrap();
        fs::write(
            &large,
            format!(
                "{}TARGET_LINE\n{}",
                format!("{}\n", "x".repeat(100)).repeat(899),
                format!("{}\n", "x".repeat(100)).repeat(100)
            ),
        )
        .unwrap();
        for target in [
            json!({"path":small,"start":500,"end":500}),
            json!({"path":small,"symbol":"tail_target"}),
            json!({"path":large,"start":900,"end":900}),
        ] {
            let out = tool()
                .call(json!({"targets":[target]}), &ToolContext::new())
                .await
                .unwrap();
            let content = out["results"][0]["file"]["content"].as_str().unwrap();
            assert!(
                content.contains("tail_target") || content.contains("TARGET_LINE"),
                "{out}"
            );
        }
        let context = ToolContext::new().with_file_state_cache(FileStateCache::new());
        let out = tool().call(json!({"targets":[{"path":small,"start":1,"end":1}, {"path":small,"start":500,"end":500}]}), &context).await.unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r["file"]["content"]
            .as_str()
            .is_some_and(|s| s.contains("tail_target"))));
        let retry = tool()
            .call(
                json!({"targets":[{"path":small,"start":500,"end":500}]}),
                &context,
            )
            .await
            .unwrap();
        assert!(retry["results"][0]["file"]["content"]
            .as_str()
            .unwrap()
            .contains("tail_target"));
    }

    #[tokio::test]
    async fn batch_expands_and_merges_text_ranges() {
        let dir = TempDir::new();
        let small = dir.path().join("small.txt");
        let large = dir.path().join("large.txt");
        fs::write(&small, "small\n".repeat(10)).unwrap();
        fs::write(&large, "line\n".repeat(1000)).unwrap();
        let result = tool()
            .call(
                json!({"targets": [
                    {"path": small, "start": 5, "end": 6},
                    {"path": large, "start": 1, "end": 3},
                    {"path": large, "start": 450, "end": 451}
                ]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["file"]["startLine"], 1);
        assert_eq!(results[0]["file"]["numLines"], 10);
        assert_eq!(results[1]["file"]["numLines"], 600);
    }

    #[tokio::test]
    async fn adjacent_reads_merge_only_when_file_is_unchanged() {
        let dir = TempDir::new();
        let path = dir.path().join("large.txt");
        fs::write(&path, "line\n".repeat(1000)).unwrap();
        let context = ToolContext::new().with_file_state_cache(FileStateCache::new());
        let first = tool()
            .call(
                json!({"targets":[{"path":path,"start":1,"end":1}]}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(first["results"][0]["file"]["numLines"], 300);
        let second = tool()
            .call(
                json!({"targets":[{"path":path,"start":450,"end":450}]}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(second["results"][0]["file"]["startLine"], 1);
        assert_eq!(second["results"][0]["file"]["numLines"], 600);
        fs::write(&path, "changed\n".repeat(1000)).unwrap();
        let changed = tool()
            .call(
                json!({"targets":[{"path":path,"start":601,"end":601}]}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(changed["results"][0]["file"]["startLine"], 452);
    }

    #[tokio::test]
    async fn narrow_window_expands_around_requested_lines() {
        let dir = TempDir::new();
        let path = dir.path().join("large.txt");
        fs::write(&path, "line\n".repeat(1000)).unwrap();
        let result = tool()
            .call(
                json!({"targets": [{"path": path, "start": 500, "end": 500}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["results"][0]["file"]["startLine"], 351);
        assert_eq!(result["results"][0]["file"]["numLines"], 300);
    }

    #[tokio::test]
    async fn batch_validates_every_target_before_reading() {
        let dir = TempDir::new();
        let file = dir.path().join("ok.txt");
        fs::write(&file, "hello").unwrap();
        let input = json!({"targets": [{"path": file}, {"path": "relative.txt"}]});
        assert!(tool().call(input, &ToolContext::new()).await.is_err());
    }

    #[tokio::test]
    async fn batch_preserves_image_payload() {
        let dir = TempDir::new();
        let path = dir.path().join("pixel.png");
        let png = BASE64.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
        fs::write(&path, &png).unwrap();
        let result = tool()
            .call(json!({"targets": [{"path": path}]}), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(result["results"][0]["type"], "image");
        assert_eq!(result["results"][0]["file"]["base64"], BASE64.encode(png));
    }

    #[tokio::test]
    async fn sub_agent_read_rejects_paths_outside_scope() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        let file = outside.path().join("secret.txt");
        fs::write(&file, "secret\n").unwrap();

        let result = tool()
            .call(
                json!({ "targets": [{"path": file.to_string_lossy()}] }),
                &ToolContext::new()
                    .with_agent_id("agent-test")
                    .with_cwd(dir.path().to_string_lossy()),
            )
            .await
            .unwrap_err();

        assert!(result.to_string().contains("outside that scope"));
    }

    #[tokio::test]
    async fn sub_agent_read_allows_paths_inside_explicit_allowed_roots() {
        let dir = TempDir::new();
        let sibling = TempDir::new();
        let file = sibling.path().join("note.txt");
        fs::write(&file, "allowed\n").unwrap();

        let result = tool()
            .call(
                json!({ "targets": [{"path": file.to_string_lossy()}] }),
                &ToolContext::new()
                    .with_agent_id("agent-test")
                    .with_cwd(dir.path().to_string_lossy())
                    .with_path_scope_roots([
                        dir.path().to_path_buf(),
                        sibling.path().to_path_buf(),
                    ]),
            )
            .await
            .unwrap();

        assert_eq!(result["results"][0]["type"], "text");
    }

    #[tokio::test]
    async fn call_reads_png_as_image_payload() {
        let dir = TempDir::new();
        let path = dir.path().join("pixel.png");
        let png = BASE64
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
            .unwrap();
        fs::write(&path, &png).unwrap();

        let result = tool()
            .call(
                json!({ "targets": [{ "path": path.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let result = &result["results"][0];
        assert_eq!(result["type"], "image");
        assert_eq!(result["file"]["type"], "image/png");
        assert_eq!(result["file"]["base64"], BASE64.encode(png));
    }

    #[tokio::test]
    async fn call_rejects_invalid_image_data_with_actionable_error() {
        let dir = TempDir::new();
        let path = dir.path().join("broken.png");
        fs::write(&path, b"not an image").unwrap();

        let error = tool()
            .call(
                json!({ "targets": [{ "path": path.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap()["results"][0]
            .clone();

        assert_eq!(error["type"], "error");
        let reason = error["error"].as_str().unwrap();
        assert!(reason.contains("contents are not valid image/png data"));
        assert!(reason.contains("incomplete or mislabeled"));
        let direct_error = validate_image_bytes(&path, "image/png", b"not an image").unwrap_err();
        assert!(matches!(
            direct_error,
            ToolError::InvalidInput {
                error_code: Some(INVALID_INPUT_CODE),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn call_rejects_image_data_that_does_not_match_extension() {
        let dir = TempDir::new();
        let path = dir.path().join("pixel.jpg");
        let png = BASE64
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
            .unwrap();
        fs::write(&path, png).unwrap();

        let error = tool()
            .call(
                json!({ "targets": [{ "path": path.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(error["results"][0]["type"], "error");
        let error = error["results"][0]["error"].as_str().unwrap();

        assert!(error.contains("contents are not valid image/jpeg data"));
        assert!(error.contains("correct its extension"));
    }

    #[tokio::test]
    async fn call_rejects_truncated_png_after_valid_signature() {
        let dir = TempDir::new();
        let path = dir.path().join("truncated.png");
        fs::write(&path, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();

        let error = tool()
            .call(
                json!({ "file_path": path.to_string_lossy() }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ToolError::InvalidInput {
                error_code: Some(INVALID_INPUT_CODE),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn call_rejects_gif_with_valid_first_frame_and_truncated_second_frame() {
        let dir = TempDir::new();
        let path = dir.path().join("truncated.gif");
        let first_frame = vec![
            b'G', b'I', b'F', b'8', b'9', b'a', 1, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff,
            0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 1, 0,
        ];
        let mut valid_gif = first_frame.clone();
        valid_gif.push(0x3b);
        validate_image_bytes(&path, "image/gif", &valid_gif).unwrap();

        let mut gif = first_frame;
        gif.extend_from_slice(&[0x2c, 0, 0]);
        fs::write(&path, gif).unwrap();

        let error = tool()
            .call(
                json!({ "file_path": path.to_string_lossy() }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ToolError::InvalidInput {
                error_code: Some(INVALID_INPUT_CODE),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn call_reads_valid_webp_as_image_payload() {
        let dir = TempDir::new();
        let path = dir.path().join("pixel.webp");
        let mut webp = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut webp, ImageFormat::WebP)
            .unwrap();
        fs::write(&path, webp.into_inner()).unwrap();

        let result = tool()
            .call(
                json!({ "targets": [{ "path": path.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let result = &result["results"][0];
        assert_eq!(result["type"], "image");
        assert_eq!(result["file"]["type"], "image/webp");
    }

    #[tokio::test]
    async fn call_rejects_oversized_image_before_reading_contents() {
        let dir = TempDir::new();
        let path = dir.path().join("oversized.png");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_IMAGE_FILE_SIZE_BYTES + 1).unwrap();

        let error = tool()
            .call(
                json!({ "targets": [{ "path": path.to_string_lossy(), "start": 1, "end": 1 }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap()["results"][0]
            .clone();

        assert_eq!(error["type"], "error");
        let reason = error["error"].as_str().unwrap();
        assert!(reason.contains("maximum safe size"));
        assert!(reason.contains("Resize or recompress"));
        let direct_error = tool()
            .read_one(
                ReadInput {
                    file_path: path,
                    offset: 1,
                    limit: Some(1),
                    pages: None,
                    symbol: None,
                },
                &ToolContext::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            direct_error,
            ToolError::InvalidInput {
                error_code: Some(FILE_TOO_LARGE_CODE),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn validate_image_rejects_dimensions_over_safe_limit() {
        let path = Path::new("too-wide.png");
        let image = image::DynamicImage::new_rgb8(MAX_IMAGE_DIMENSION + 1, 1);
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, ImageFormat::Png).unwrap();

        let error = validate_image_bytes(path, "image/png", png.get_ref()).unwrap_err();

        assert!(matches!(
            error,
            ToolError::InvalidInput {
                error_code: Some(INVALID_INPUT_CODE),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn validate_input_accepts_pdf_pages_range() {
        let dir = TempDir::new();
        let file = dir.path().join("doc.pdf");
        fs::write(&file, b"%PDF-1.4\n").unwrap();

        let result = tool()
            .validate_input(
                &json!({ "targets": [{"path": file.to_string_lossy(), "pages": "1-5"}] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert!(
            result.is_valid(),
            "unexpected validation result: {result:?}"
        );
    }

    #[tokio::test]
    async fn validate_input_accepts_pdf_open_ended_pages_range() {
        let dir = TempDir::new();
        let file = dir.path().join("doc.pdf");
        fs::write(&file, b"%PDF-1.4\n").unwrap();

        let result = tool()
            .validate_input(
                &json!({ "targets": [{"path": file.to_string_lossy(), "pages": "10-"}] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert!(
            result.is_valid(),
            "unexpected validation result: {result:?}"
        );
    }

    #[tokio::test]
    async fn validate_input_rejects_invalid_pdf_pages_range() {
        let dir = TempDir::new();
        let file = dir.path().join("doc.pdf");
        fs::write(&file, b"%PDF-1.4\n").unwrap();

        let result = tool()
            .validate_input(
                &json!({ "targets": [{"path": file.to_string_lossy(), "pages": "5-1"}] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert!(!result.is_valid());
        assert_eq!(result.error_code, Some(INVALID_PAGES_CODE));
    }

    #[tokio::test]
    async fn validate_input_rejects_pdf_pages_range_over_limit() {
        let dir = TempDir::new();
        let file = dir.path().join("doc.pdf");
        fs::write(&file, b"%PDF-1.4\n").unwrap();

        let result = tool()
            .validate_input(
                &json!({ "targets": [{"path": file.to_string_lossy(), "pages": "1-21"}] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert!(!result.is_valid());
        assert_eq!(result.error_code, Some(PAGE_RANGE_TOO_LARGE_CODE));
    }

    #[tokio::test]
    async fn call_reads_small_pdf_as_pdf_payload() {
        let dir = TempDir::new();
        let file = dir.path().join("doc.pdf");
        let pdf = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n";
        fs::write(&file, pdf).unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let out = &out["results"][0];
        assert_eq!(out["type"], "pdf");
        assert_eq!(out["file"]["base64"], BASE64.encode(pdf));
        assert_eq!(out["file"]["originalSize"], json!(pdf.len()));
    }

    #[tokio::test]
    async fn call_rejects_invalid_pdf_header() {
        let dir = TempDir::new();
        let file = dir.path().join("fake.pdf");
        fs::write(&file, b"not a pdf").unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let result = &out["results"][0];
        assert_eq!(result["type"], "error");
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("missing %PDF- header"));
    }

    #[tokio::test]
    async fn call_rejects_pdf_pages_when_pdftoppm_is_unavailable() {
        // Crate-wide lock: blanking `PATH` here would otherwise stop
        // unrelated tests from spawning `git` or an MCP server.
        let _guard = crate::test_env::lock_env();
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "");

        let dir = TempDir::new();
        let file = dir.path().join("doc.pdf");
        fs::write(&file, b"%PDF-1.4\n").unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy(), "pages": "1" }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }

        let result = &out["results"][0];
        assert_eq!(result["type"], "error");
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("pdftoppm is not installed"));
    }

    #[tokio::test]
    async fn call_rejects_invalid_pdf_pages_header_before_pdftoppm_check() {
        // Crate-wide lock: blanking `PATH` here would otherwise stop
        // unrelated tests from spawning `git` or an MCP server.
        let _guard = crate::test_env::lock_env();
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "");

        let dir = TempDir::new();
        let file = dir.path().join("fake.pdf");
        fs::write(&file, b"not a pdf").unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy(), "pages": "1" }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }

        let result = &out["results"][0];
        assert_eq!(result["type"], "error");
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("missing %PDF- header"));
    }

    #[tokio::test]
    async fn sorts_pdf_page_images_by_numeric_suffix() {
        let dir = TempDir::new();
        let paths = ["page-10.jpg", "page-2.jpg", "page-1.jpg"]
            .into_iter()
            .map(|name| dir.path().join(name))
            .collect::<Vec<_>>();
        let mut sorted = paths.clone();

        sorted.sort_by(|a, b| pdf_page_image_sort_key(a).cmp(&pdf_page_image_sort_key(b)));

        let names = sorted
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["page-1.jpg", "page-2.jpg", "page-10.jpg"]);
    }

    #[tokio::test]
    async fn zero_start_is_rejected() {
        let dir = TempDir::new();
        let file = dir.path().join("zero.txt");
        fs::write(&file, "alpha\nbeta").unwrap();

        let err = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy(), "start": 0 }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ToolError::InvalidInput {
                error_code: Some(INVALID_INPUT_CODE),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn image_read_accepts_explicit_range() {
        let dir = TempDir::new();
        let path = dir.path().join("pixel.jpg");
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .unwrap();
        fs::write(&path, jpeg.into_inner()).unwrap();

        let result = tool()
            .call(
                json!({ "targets": [{ "path": path.to_string_lossy(), "start": 1 }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let result = &result["results"][0];
        assert_eq!(result["type"], "image");
        assert_eq!(result["file"]["type"], "image/jpeg");
    }

    #[tokio::test]
    async fn validate_input_rejects_relative_path() {
        let input = json!({ "targets": [{"path": "src/lib.rs"}] });
        let result = tool()
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap();
        assert!(!result.is_valid());
        assert_eq!(result.error_code, Some(400));
    }

    #[tokio::test]
    async fn validate_input_rejects_directory() {
        let dir = TempDir::new();
        let input = json!({ "targets": [{"path": dir.path().to_string_lossy()}] });
        let result = tool()
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(
            result,
            ValidationOutcome::invalid(format!("Path is not a file: {}", dir.path().display()), 2)
        );
    }

    #[tokio::test]
    async fn call_reads_text_file_with_line_numbers() {
        let dir = TempDir::new();
        let file = dir.path().join("demo.txt");
        fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "targets": [{ "path": file.to_string_lossy(), "start": 2, "end": 3 }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let out = &out["results"][0];
        assert_eq!(out["type"], json!("text"));
        assert_eq!(out["file"]["numLines"], json!(3));
        assert_eq!(out["file"]["startLine"], json!(1));
        assert_eq!(out["file"]["totalLines"], json!(3));
        assert_eq!(
            out["file"]["content"],
            json!("     1\talpha\n     2\tbeta\n     3\tgamma")
        );
    }

    #[tokio::test]
    async fn call_rejects_non_utf8_binary_slice() {
        let dir = TempDir::new();
        let file = dir.path().join("blob.bin");
        fs::write(&file, [0_u8, 159, 146, 150]).unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let result = &out["results"][0];
        assert_eq!(result["type"], "error");
        assert!(result["error"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("utf-8"));
    }

    #[tokio::test]
    async fn call_truncates_large_file_without_range() {
        let dir = TempDir::new();
        let file = dir.path().join("big.txt");
        let big_content = "x".repeat((MAX_FILE_SIZE_BYTES as usize) + 1);
        fs::write(&file, &big_content).unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let result = &out["results"][0];
        assert_eq!(result["type"], "text");
        assert_eq!(result["file"]["numLines"], 1);
        assert_eq!(result["file"]["totalLines"], 1);
        let content = result["file"]["content"].as_str().unwrap();
        assert!(content.len() <= READ_MAX_OUTPUT_BYTES);
        assert!(content.contains("[line clipped:"));
        assert!(result["truncation"]
            .as_str()
            .unwrap()
            .contains("Long lines clipped"));
    }

    #[tokio::test]
    async fn call_allows_large_file_with_explicit_limit() {
        let dir = TempDir::new();
        let file = dir.path().join("big_ranged.txt");
        let lines: Vec<String> = (1..=5000).map(|i| format!("line {i}")).collect();
        fs::write(&file, lines.join("\n")).unwrap();

        // A narrow range expands to the minimum 300-line window.
        let out = tool()
            .call(
                json!({
                    "targets": [{ "path": file.to_string_lossy(), "start": 1, "end": 10 }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let out = &out["results"][0];
        assert_eq!(out["type"], "text");
        assert_eq!(out["file"]["numLines"], json!(300));
        assert_eq!(out["file"]["startLine"], json!(1));
        assert_eq!(out["file"]["totalLines"], json!(5000));
        assert!(out["file"]["content"]
            .as_str()
            .unwrap()
            .contains("line 300"));
    }

    #[tokio::test]
    async fn call_truncates_when_total_output_exceeds_byte_budget() {
        let dir = TempDir::new();
        let file = dir.path().join("byte_heavy.txt");
        // 80 lines of 1 KiB each → 80 KiB total, well over the 16 KiB
        // budget. Per-line clipping does not trigger because each line
        // is below READ_MAX_LINE_BYTES.
        let line = "a".repeat(1024);
        let lines: Vec<&str> = (0..80).map(|_| line.as_str()).collect();
        fs::write(&file, lines.join("\n")).unwrap();

        let out = tool()
            .call(
                json!({
                    "targets": [{ "path": file.to_string_lossy(), "start": 1, "end": 80 }],
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let out = &out["results"][0];
        let content = out["file"]["content"].as_str().unwrap();
        assert!(
            content.len() <= READ_MAX_OUTPUT_BYTES,
            "rendered content {} bytes must fit byte budget {}",
            content.len(),
            READ_MAX_OUTPUT_BYTES,
        );
        let emitted = out["file"]["numLines"].as_u64().unwrap() as usize;
        assert!(
            emitted > 0 && emitted < 80,
            "expected partial emit, got {emitted}"
        );

        let note = out["truncation"]
            .as_str()
            .expect("byte-budget truncation must surface a note");
        assert!(
            note.contains("byte budget"),
            "note should explain budget: {note}"
        );
        assert!(
            note.contains("Hint:"),
            "note should include retry hint: {note}"
        );
    }

    #[tokio::test]
    async fn call_clips_single_long_line_with_marker() {
        let dir = TempDir::new();
        let file = dir.path().join("long_line.txt");
        // Write one 64 KiB line surrounded by short lines.
        let huge = "x".repeat(64 * 1024);
        let body = format!("alpha\n{huge}\ngamma");
        fs::write(&file, body).unwrap();

        let out = tool()
            .call(
                json!({
                    "targets": [{ "path": file.to_string_lossy(), "start": 1, "end": 3 }],
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let out = &out["results"][0];
        let content = out["file"]["content"].as_str().unwrap();
        assert!(
            content.len() <= READ_MAX_OUTPUT_BYTES,
            "output must respect byte budget after clipping"
        );
        assert!(
            content.contains("[line clipped:"),
            "long line should carry a clip marker: {content}"
        );
        assert!(
            content.contains("alpha"),
            "short head line must be preserved"
        );

        let note = out["truncation"]
            .as_str()
            .expect("clipping must surface a truncation note");
        assert!(
            note.contains("Long lines clipped"),
            "note should mention line clipping: {note}"
        );
    }

    #[tokio::test]
    async fn line_clip_preserves_utf8_char_boundaries() {
        // The clipped string must remain valid UTF-8 even when the
        // raw byte cut would slice through a 3-byte CJK char.
        let mut line = String::new();
        // Repeat a 3-byte char 3000 times → 9000 bytes, comfortably
        // over the 4 KiB line cap, with no boundaries at 1024/1024.
        for _ in 0..3000 {
            line.push('中');
        }
        let (clipped, omitted) = clip_long_line(&line);
        assert!(omitted > 0, "expected omission for 9KB line");
        assert!(
            clipped.is_char_boundary(0) && clipped.is_char_boundary(clipped.len()),
            "clipped output must be valid UTF-8"
        );
        // Round-trip through str/String validates UTF-8.
        let _ = String::from(clipped.as_str());
    }

    #[tokio::test]
    async fn parallel_reads_each_respect_byte_budget() {
        // Simulates the failure mode the byte cap was designed for:
        // three parallel Reads over a file with extremely long lines
        // should each stay within the budget instead of cumulatively
        // adding hundreds of KB to the transcript.
        let dir = TempDir::new();
        let file = dir.path().join("obfuscated.js");
        // 1500 lines of 800 bytes each → similar to the minified
        // file shape that originally triggered the issue.
        let line = "y".repeat(800);
        let lines: Vec<&str> = (0..1500).map(|_| line.as_str()).collect();
        fs::write(&file, lines.join("\n")).unwrap();

        let tool = tool();
        let ctx = ToolContext::new();
        let calls = [
            json!({ "targets": [{ "path": file.to_string_lossy(), "start": 1, "end": 80 }] }),
            json!({ "targets": [{ "path": file.to_string_lossy(), "start": 500, "end": 579 }] }),
            json!({ "targets": [{ "path": file.to_string_lossy(), "start": 1000, "end": 1079 }] }),
        ];
        let mut total_bytes = 0usize;
        for input in calls {
            let out = tool.call(input, &ctx).await.unwrap();
            let out = &out["results"][0];
            assert_eq!(out["type"], "text");
            let content = out["file"]["content"].as_str().unwrap();
            assert!(
                content.len() <= READ_MAX_OUTPUT_BYTES,
                "each Read must fit byte budget"
            );
            total_bytes += content.len();
        }
        assert!(
            total_bytes <= 3 * READ_MAX_OUTPUT_BYTES,
            "three parallel Reads must add at most 3 × {} bytes",
            READ_MAX_OUTPUT_BYTES
        );
    }

    #[tokio::test]
    async fn auto_truncation_note_no_longer_errors_on_huge_explicit_limit() {
        // Before the byte budget existed, this scenario hit the old
        // 25k-token gate and returned an error, wasting a model turn.
        // Now it should return a partial slice with metadata so the
        // model can keep working.
        let dir = TempDir::new();
        let file = dir.path().join("explicit_limit.txt");
        let line = "z".repeat(200);
        let lines: Vec<&str> = (0..600).map(|_| line.as_str()).collect();
        fs::write(&file, lines.join("\n")).unwrap();

        let out = tool()
            .call(
                json!({
                    "targets": [{ "path": file.to_string_lossy(), "start": 1, "end": 600 }],
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let out = &out["results"][0];

        assert_eq!(out["type"], "text");
        let note = out["truncation"].as_str().unwrap();
        assert!(note.contains("Returned lines"));
    }

    #[tokio::test]
    async fn call_auto_truncates_large_file_without_limit() {
        let dir = TempDir::new();
        let file = dir.path().join("many_lines.txt");
        let lines: Vec<String> = (1..=700).map(|i| format!("line {i}")).collect();
        fs::write(&file, lines.join("\n")).unwrap();

        // No range provided selects the first 300 lines of a large file.
        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let out = &out["results"][0];

        assert_eq!(out["file"]["numLines"], json!(300));
        assert_eq!(out["file"]["totalLines"], json!(700));
        assert_eq!(out["file"]["startLine"], 1);
        assert!(out["file"]["content"]
            .as_str()
            .unwrap()
            .contains("line 300"));
        assert!(!out["file"]["content"]
            .as_str()
            .unwrap()
            .contains("line 301"));
    }

    #[tokio::test]
    async fn read_registers_full_view_into_cache() {
        let dir = TempDir::new();
        let file = dir.path().join("full.txt");
        fs::write(&file, "hello\nworld").unwrap();
        let cache = FileStateCache::new();
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ctx,
            )
            .await
            .unwrap();

        let state = cache.get(&file).expect("file should be registered");
        // RAW disk content — not the rendered version with line numbers.
        assert_eq!(state.content, "hello\nworld");
        assert!(!state.is_partial_view);
        assert!(state.offset.is_none());
        assert!(state.limit.is_none());
        assert!(state.timestamp_ms > 0);
    }

    #[tokio::test]
    async fn read_records_offset_and_limit_without_partial_view_flag() {
        let dir = TempDir::new();
        let file = dir.path().join("slice.txt");
        let content = (1..=700)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, &content).unwrap();
        let cache = FileStateCache::new();
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(
                json!({
                    "targets": [{ "path": file.to_string_lossy(), "start": 2, "end": 3 }]
                }),
                &ctx,
            )
            .await
            .unwrap();

        let state = cache.get(&file).expect("file should be registered");
        // The expanded offset/limit are recorded so the Read dedup path
        // can tell a Read-populated entry
        // apart from a post-Edit refresh. They do NOT imply a partial
        // view — the model saw exactly these lines from disk.
        assert!(!state.is_partial_view);
        assert_eq!(state.offset, Some(1));
        assert_eq!(state.limit, Some(300));
        // Raw disk content is still stored so Edit's content
        // comparison works without a follow-up full Read.
        assert_eq!(state.content, content);
    }

    #[tokio::test]
    async fn read_does_not_flag_partial_view_on_auto_truncation() {
        let dir = TempDir::new();
        let file = dir.path().join("many.txt");
        let lines: Vec<String> = (1..=700).map(|i| format!("line {i}")).collect();
        fs::write(&file, lines.join("\n")).unwrap();
        let cache = FileStateCache::new();
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ctx,
            )
            .await
            .unwrap();

        let state = cache.get(&file).expect("file should be registered");
        assert!(
            !state.is_partial_view,
            "auto-truncation is a Read-path concern — `is_partial_view` is reserved \
             for auto-injected REBON.md / MEMORY.md content that differs from disk"
        );
        assert_eq!(
            state.offset,
            Some(1),
            "auto-truncated reads still record the line range for dedup"
        );
        assert_eq!(state.limit, Some(300));
    }

    #[tokio::test]
    async fn coordinator_mode_allows_registered_report_path() {
        let dir = TempDir::new();
        let report = dir.path().join("worker.report.md");
        fs::write(&report, "## Summary\n\nDone.\n").unwrap();
        let ctx = ToolContext::new()
            .with_coordinator_mode(true)
            .with_coordinator_report_paths(vec![report.clone()]);

        let out = tool()
            .call(
                json!({ "targets": [{"path": report.to_string_lossy()}] }),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(out["results"][0]["file"]["totalLines"], json!(3));
    }

    #[tokio::test]
    async fn coordinator_mode_rejects_non_allowlisted_existing_path() {
        let dir = TempDir::new();
        let report = dir.path().join("worker.report.md");
        let source = dir.path().join("source.rs");
        fs::write(&report, "## Summary\n\nDone.\n").unwrap();
        fs::write(&source, "fn main() {}\n").unwrap();
        let ctx = ToolContext::new()
            .with_coordinator_mode(true)
            .with_coordinator_report_paths(vec![report]);

        let err = tool()
            .call(
                json!({ "targets": [{"path": source.to_string_lossy()}] }),
                &ctx,
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput {
                reason, error_code, ..
            } => {
                assert_eq!(error_code, Some(INVALID_INPUT_CODE));
                assert!(
                    reason.contains("registered worker report files"),
                    "{reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn normal_context_ignores_report_allowlist() {
        let dir = TempDir::new();
        let source = dir.path().join("source.rs");
        fs::write(&source, "fn main() {}\n").unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{"path": source.to_string_lossy()}] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["results"][0]["file"]["numLines"], json!(1));
    }

    #[tokio::test]
    async fn coordinator_mode_canonicalizes_allowlisted_report_path() {
        let dir = TempDir::new();
        let nested = dir.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        let report = nested.join("worker.report.md");
        fs::write(&report, "## Summary\n\nDone.\n").unwrap();
        let bypass = nested.join("..").join("nested").join("worker.report.md");
        let ctx = ToolContext::new()
            .with_coordinator_mode(true)
            .with_coordinator_report_paths(vec![report]);

        let out = tool()
            .call(
                json!({ "targets": [{"path": bypass.to_string_lossy()}] }),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(out["results"][0]["file"]["totalLines"], json!(3));
    }

    #[tokio::test]
    async fn batch_rejects_text_over_memory_limit() {
        let dir = TempDir::new();
        let file = dir.path().join("oversized.txt");
        fs::write(&file, vec![b'x'; 16 * 1024 * 1024 + 1]).unwrap();
        let out = tool()
            .call(
                json!({"targets": [{"path": file.to_string_lossy(), "start": 1, "end": 2}]}),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(out["results"][0]["type"], "error");
        assert!(out["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("16 MiB"));
    }

    #[tokio::test]
    async fn call_reads_small_file_fully_without_limit() {
        let dir = TempDir::new();
        let file = dir.path().join("small.txt");
        let lines: Vec<String> = (1..=50).map(|i| format!("line {i}")).collect();
        fs::write(&file, lines.join("\n")).unwrap();

        let out = tool()
            .call(
                json!({ "targets": [{ "path": file.to_string_lossy() }] }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        let out = &out["results"][0];

        assert_eq!(out["file"]["numLines"], json!(50));
        assert_eq!(out["file"]["totalLines"], json!(50));
        assert!(
            out.get("truncation").is_none(),
            "small file should not be truncated"
        );
    }
}
