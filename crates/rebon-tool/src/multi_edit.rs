//! `MultiEdit` — several exact string replacements against one file,
//! applied atomically.
//!
//! Semantically this is `Edit` run N times, with two differences that
//! matter:
//!
//! * **Sequential.** Edit `i` sees the text produced by edits `0..i`,
//!   so a later edit may legitimately match text an earlier one wrote.
//! * **Atomic.** Every edit is applied to an in-memory buffer first.
//!   If any one of them fails, nothing is written — the file on disk is
//!   exactly as it was, and the error names the failing index. A
//!   half-applied multi-edit is the failure mode this tool exists to
//!   prevent, so there is no partial-success path.
//!
//! Permission, sub-agent write scope, must-read-before-edit, and
//! file-history tracking all reuse `Edit`'s implementations, which is
//! what puts MultiEdit in the same permission class: an `Edit(path)`
//! allow/deny rule and `acceptEdits` both cover it.

use crate::edit::{
    check_read_before_edit, check_read_before_edit_with_state, enforce_edit_path_scope,
    explicitly_authorized_write_path, is_auto_memory_path, normalize_line_endings, normalize_path,
    AMBIGUOUS_MATCH_CODE, FILE_MODIFIED_SINCE_READ_CODE, IDENTICAL_STRINGS_CODE,
    INVALID_INPUT_CODE, NOT_FILE_CODE, NOT_FOUND_CODE, OLD_STRING_NOT_FOUND_CODE,
    UNSUPPORTED_MEDIA_CODE,
};
use crate::{Tool, ToolContext};
use async_trait::async_trait;
use rebon_tools_core::{
    file_state::{file_mtime_ms, FileState},
    require_valid_input, validation_outcome_from, PermissionDecision, PermissionRequest, ToolError,
    ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE_MULTI_EDIT_TOOL_NAME: &str = "MultiEdit";

/// The `edits` array was missing, empty, or not an array. Distinct from
/// the generic input code so the model can tell "you sent no edits"
/// apart from "your path is relative".
const NO_EDITS_CODE: i64 = 9;

#[derive(Debug, Clone, Default)]
pub struct MultiEditTool;

#[derive(Debug, Clone)]
struct SingleEdit {
    old_string: String,
    new_string: String,
    replace_all: bool,
}

#[derive(Debug, Clone)]
struct MultiEditInput {
    file_path: PathBuf,
    edits: Vec<SingleEdit>,
}

impl MultiEditInput {
    /// The `old_string` the read-before-edit precondition keys off: the
    /// first edit's, because that is the one matched against the file as
    /// it exists on disk. An empty one means "create this file".
    fn leading_old_string(&self) -> &str {
        self.edits
            .first()
            .map(|edit| edit.old_string.as_str())
            .unwrap_or("")
    }

    fn creates_file(&self) -> bool {
        self.leading_old_string().is_empty()
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    fn id(&self) -> ToolId {
        ToolId::new(FILE_MULTI_EDIT_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["MultiEditTool", "FileMultiEditTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::FileEdit
    }

    fn file_target_field(&self) -> Option<&'static str> {
        Some("file_path")
    }

    fn description(&self) -> &str {
        "Applies several exact string replacements to a single file in one atomic operation.\n\
         \n\
         Prefer this over repeated `Edit` calls when you are making multiple changes to the \
         same file — it is one permission prompt, one write, and one diff.\n\
         \n\
         Usage:\n\
         - You must use your `Read` tool at least once in the conversation before editing. \
         This tool will error if you attempt an edit without reading the file.\n\
         - A same-file Read and MultiEdit, or multiple mutations of the same file, are dependent \
         operations and must not be sent in one parallel tool batch. If this tool reports \
         modified-since-read, Read the file again and preserve the latest contents before retrying.\n\
         - Provide `file_path` (absolute) and `edits`, a non-empty array of \
         `{old_string, new_string, replace_all?}` objects with the same matching rules as `Edit`.\n\
         - Edits are applied IN ORDER, and each one operates on the result of the previous one. \
         Build every `old_string` against the text as it will exist at that point, not against \
         the original file. Sequential edits that rewrite the same region must be written to chain.\n\
         - ATOMIC: if any edit fails (its `old_string` is not found, matches more than once \
         without `replace_all`, or equals its `new_string`), NOTHING is written and the error \
         reports which index failed. Fix that edit and resend the whole call.\n\
         - Each `old_string` must be unique in the text it is applied to, unless `replace_all` \
         is true. Add surrounding context to disambiguate, or set `replace_all` to rename every \
         occurrence.\n\
         - To create a new file: pass a single first edit with an empty `old_string` and the \
         file's full contents as `new_string`.\n\
         - When editing text from Read tool output, preserve the exact indentation as it appears \
         AFTER the line number prefix. Never include any part of the line number prefix in \
         `old_string` or `new_string`.\n\
         - Only use emojis if the user explicitly requests it."
    }

    fn search_hint(&self) -> Option<&str> {
        Some("batch atomic multiple replacements one file")
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Replacements to apply in order. Each edit sees the text produced by the previous edits, so later `old_string`s must match the intermediate state, not the original file.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {
                                "type": "string",
                                "description": "The exact text to replace. Empty only in the first edit, to create a new file."
                            },
                            "new_string": {
                                "type": "string",
                                "description": "The text to replace it with; must differ from old_string"
                            },
                            "replace_all": {
                                "type": "boolean",
                                "description": "Replace every occurrence instead of requiring a unique match (default false)"
                            }
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["file_path", "edits"],
            "additionalProperties": false
        })
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match prepare_input(self.id(), input, context) {
            Ok(parsed) => {
                check_read_before_edit(&parsed.file_path, parsed.leading_old_string(), context)
            }
            refused => validation_outcome_from(refused),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");

        if explicitly_authorized_write_path(Path::new(path), context) {
            return Ok(PermissionDecision::allow(input.clone()));
        }

        // Auto-allow edits to the auto-memory directory unless the
        // context carries an explicit narrower write scope.
        if context.write_scope_roots().is_none() {
            if let Some(cwd) = context.cwd() {
                if rebon_session::memory_paths::is_memory_path_for_any_scope(path, cwd) {
                    return Ok(PermissionDecision::allow(input.clone()));
                }
            }
        }

        let count = input
            .get("edits")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let plural = if count == 1 { "edit" } else { "edits" };
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Edit file",
                format!("MultiEdit wants to apply {count} {plural} to: {path}"),
            )
            .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = prepare_input(self.id(), &input, context)?;

        let observed_state = context
            .file_state_cache()
            .and_then(|cache| cache.get(&parsed.file_path));
        let _file_guard = crate::lock_file_for_write(&parsed.file_path).await;
        crate::ensure_file_not_mutated_in_current_batch(
            &parsed.file_path,
            context,
            self.id(),
            FILE_MODIFIED_SINCE_READ_CODE,
        )?;

        let state_check = check_read_before_edit_with_state(
            &parsed.file_path,
            parsed.leading_old_string(),
            context,
            observed_state,
        )?;
        if !state_check.is_valid() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: state_check
                    .message
                    .unwrap_or_else(|| "MultiEdit precondition failed".into()),
                error_code: state_check.error_code,
            });
        }

        let original = match fs::read(&parsed.file_path) {
            Ok(bytes) => Some(
                String::from_utf8(bytes).map_err(|_| ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!(
                        "Unsupported non-text file in current Rust MultiEdit slice: {}",
                        parsed.file_path.display()
                    ),
                    error_code: Some(UNSUPPORTED_MEDIA_CODE),
                })?,
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(ToolError::Execution {
                    tool: self.id(),
                    source: err.into(),
                })
            }
        };

        if original.is_none() && !parsed.creates_file() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: format!(
                    "File does not exist: {}. Use an empty `old_string` in the first edit only \
                     when creating a new file via MultiEdit.",
                    parsed.file_path.display()
                ),
                error_code: Some(NOT_FOUND_CODE),
            });
        }

        // Apply every edit to an in-memory buffer. Any failure returns
        // before a single byte reaches disk.
        let (updated, per_edit) = apply_edits(&self.id(), original.as_deref(), &parsed.edits)?;
        let edit_type = if original.is_some() {
            "update"
        } else {
            "create"
        };

        if let Some(parent) = parsed.file_path.parent() {
            fs::create_dir_all(parent).map_err(|err| ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            })?;
        }

        if !is_auto_memory_path(&parsed.file_path, context) {
            if let Some(tracker) = context.file_history_tracker() {
                tracker
                    .track_before_write(&parsed.file_path)
                    .map_err(|err| ToolError::Execution {
                        tool: self.id(),
                        source: err,
                    })?;
            }
        }

        fs::write(&parsed.file_path, updated.as_bytes()).map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;
        crate::record_file_mutation_in_current_batch(&parsed.file_path, context);

        // Refresh the cache with (new content, new mtime) so a
        // follow-up Edit / MultiEdit on the same file does not trip
        // the modified-since-read guard.
        if let Some(cache) = context.file_state_cache() {
            let timestamp_ms = file_mtime_ms(&parsed.file_path).unwrap_or(0);
            cache.set(
                &parsed.file_path,
                FileState {
                    content: updated.clone(),
                    timestamp_ms,
                    offset: None,
                    limit: None,
                    is_partial_view: false,
                },
            );
        }

        // `oldString` / `newString` carry the narrowest span of the file
        // that actually changed, so the shared diff renderer (which
        // locates `oldString` inside `originalFile` and pads it with
        // context) shows one coherent hunk instead of the whole file.
        let (old_span, new_span) = changed_span(original.as_deref().unwrap_or(""), &updated);
        let replacements: usize = per_edit.iter().map(|edit| edit.replacements).sum();

        let result = json!({
            "type": edit_type,
            "filePath": normalize_path(&parsed.file_path),
            "oldString": old_span,
            "newString": new_span,
            "editCount": parsed.edits.len(),
            "replacements": replacements,
            "edits": per_edit
                .iter()
                .map(|edit| json!({
                    "old_string": edit.old_string,
                    "new_string": edit.new_string,
                    "replace_all": edit.replace_all,
                    "replacements": edit.replacements,
                }))
                .collect::<Vec<_>>(),
            "content": updated,
            "originalFile": original,
        });

        Ok(result)
    }
}

#[derive(Debug, Clone)]
struct AppliedEdit {
    old_string: String,
    new_string: String,
    replace_all: bool,
    replacements: usize,
}

/// Apply `edits` in order to `original`, returning the final text and a
/// per-edit record. Returns `Err` on the first edit that cannot be
/// applied; the caller has written nothing at that point.
fn apply_edits(
    tool: &ToolId,
    original: Option<&str>,
    edits: &[SingleEdit],
) -> ToolResult<(String, Vec<AppliedEdit>)> {
    let mut buffer = original.unwrap_or("").to_string();
    let mut applied = Vec::with_capacity(edits.len());

    for (index, edit) in edits.iter().enumerate() {
        if original.is_none() && index == 0 {
            // Creating the file: the first edit's `new_string` is the
            // whole initial body, with nothing to match against.
            buffer = edit.new_string.clone();
            applied.push(AppliedEdit {
                old_string: edit.old_string.clone(),
                new_string: edit.new_string.clone(),
                replace_all: edit.replace_all,
                replacements: 1,
            });
            continue;
        }

        if edit.old_string.is_empty() {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!(
                    "edits[{index}]: an empty `old_string` is only valid as the first edit of a \
                     new file. To insert text into an existing file, match the surrounding text."
                ),
                error_code: Some(OLD_STRING_NOT_FOUND_CODE),
            });
        }

        let uses_crlf = buffer.contains("\r\n");
        let old_str = normalize_line_endings(&edit.old_string, uses_crlf);
        let new_str = normalize_line_endings(&edit.new_string, uses_crlf);

        let occurrences = buffer.matches(&*old_str).count();
        if occurrences == 0 {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!(
                    "edits[{index}]: `old_string` was not found. Nothing was written — the file \
                     is unchanged. Remember each edit applies to the result of the previous \
                     ones, so this `old_string` must match the text as edits[0..{index}] leave it."
                ),
                error_code: Some(OLD_STRING_NOT_FOUND_CODE),
            });
        }
        if occurrences > 1 && !edit.replace_all {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!(
                    "edits[{index}]: `old_string` matched {occurrences} times; set `replace_all` \
                     to true or add surrounding context to make it unique. Nothing was written — \
                     the file is unchanged."
                ),
                error_code: Some(AMBIGUOUS_MATCH_CODE),
            });
        }

        buffer = if edit.replace_all {
            buffer.replace(&*old_str, &*new_str)
        } else {
            buffer.replacen(&*old_str, &*new_str, 1)
        };
        applied.push(AppliedEdit {
            old_string: edit.old_string.clone(),
            new_string: edit.new_string.clone(),
            replace_all: edit.replace_all,
            replacements: if edit.replace_all { occurrences } else { 1 },
        });
    }

    Ok((buffer, applied))
}

/// Byte offset of each line start in `s`. A trailing newline does not
/// open a new line, and an empty string has no lines at all.
fn line_starts(s: &str) -> Vec<usize> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0usize];
    for (index, byte) in s.bytes().enumerate() {
        if byte == b'\n' && index + 1 < s.len() {
            starts.push(index + 1);
        }
    }
    starts
}

/// Line `index` of `s`, terminator included, given its `starts` table.
fn line_at<'a>(s: &'a str, starts: &[usize], index: usize) -> &'a str {
    let start = starts[index];
    let end = starts.get(index + 1).copied().unwrap_or(s.len());
    &s[start..end]
}

/// Narrow `(original, updated)` to the smallest span that differs, by
/// dropping the common leading and trailing *lines*. Both halves stay
/// substrings of their input, so the shared diff renderer can locate
/// the old span inside `originalFile` and pad it with context.
///
/// Comparison is line-aligned rather than byte-aligned: a byte-level
/// common-prefix scan can stop in the middle of a multi-byte character
/// (`旧` and `新` share their leading byte), and slicing a `str` there
/// panics.
fn changed_span(original: &str, updated: &str) -> (String, String) {
    if original == updated {
        return (String::new(), String::new());
    }
    let original_starts = line_starts(original);
    let updated_starts = line_starts(updated);
    let max_common = original_starts.len().min(updated_starts.len());

    let mut leading = 0;
    while leading < max_common
        && line_at(original, &original_starts, leading)
            == line_at(updated, &updated_starts, leading)
    {
        leading += 1;
    }

    let mut trailing = 0;
    while trailing < max_common - leading
        && line_at(
            original,
            &original_starts,
            original_starts.len() - trailing - 1,
        ) == line_at(
            updated,
            &updated_starts,
            updated_starts.len() - trailing - 1,
        )
    {
        trailing += 1;
    }

    let span = |text: &str, starts: &[usize], from: usize, to: usize| -> String {
        let start = starts.get(from).copied().unwrap_or(text.len());
        let end = starts.get(to).copied().unwrap_or(text.len());
        text[start.min(end)..end].to_string()
    };

    (
        span(
            original,
            &original_starts,
            leading,
            original_starts.len() - trailing,
        ),
        span(
            updated,
            &updated_starts,
            leading,
            updated_starts.len() - trailing,
        ),
    )
}

/// Parse and vet one `MultiEdit` request, once.
///
/// Same three questions in the same order for both entry points — does the
/// input parse, does the path clear the edit scope, is the parsed request
/// well-formed — after which `validate_input` answers read-before-edit from
/// the cache and `call` answers it against the state it is about to write.
fn prepare_input(tool: ToolId, input: &Value, context: &ToolContext) -> ToolResult<MultiEditInput> {
    let parsed = parse_input(input)?;
    enforce_edit_path_scope(tool.clone(), &parsed.file_path, context)?;
    require_valid_input(
        tool,
        validate_parsed_input(&parsed)?,
        "MultiEdit input is invalid",
    )?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<MultiEditInput> {
    let tool = ToolId::new(FILE_MULTI_EDIT_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "MultiEdit input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let file_path = object
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "MultiEdit input requires a string `file_path`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

    let raw_edits = object
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "MultiEdit input requires an `edits` array of \
                     {old_string, new_string, replace_all?} objects"
                .into(),
            error_code: Some(NO_EDITS_CODE),
        })?;

    if raw_edits.is_empty() {
        return Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "MultiEdit requires at least one edit in `edits`".into(),
            error_code: Some(NO_EDITS_CODE),
        });
    }

    let mut edits = Vec::with_capacity(raw_edits.len());
    for (index, raw) in raw_edits.iter().enumerate() {
        let object = raw.as_object().ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("edits[{index}] must be an object"),
            error_code: Some(INVALID_INPUT_CODE),
        })?;
        let old_string = object
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!("edits[{index}] requires a string `old_string`"),
                error_code: Some(INVALID_INPUT_CODE),
            })?;
        let new_string = object
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!("edits[{index}] requires a string `new_string`"),
                error_code: Some(INVALID_INPUT_CODE),
            })?;
        let replace_all = match object.get("replace_all") {
            Some(Value::Bool(raw)) => *raw,
            Some(Value::Null) | None => false,
            Some(_) => {
                return Err(ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: format!("edits[{index}].replace_all must be a boolean when provided"),
                    error_code: Some(INVALID_INPUT_CODE),
                })
            }
        };
        edits.push(SingleEdit {
            old_string: old_string.to_owned(),
            new_string: new_string.to_owned(),
            replace_all,
        });
    }

    Ok(MultiEditInput {
        file_path: PathBuf::from(file_path),
        edits,
    })
}

fn validate_parsed_input(input: &MultiEditInput) -> ToolResult<ValidationOutcome> {
    if !input.file_path.is_absolute() {
        return Ok(ValidationOutcome::invalid(
            format!(
                "MultiEdit requires an absolute `file_path`, got: {}. \
                 Examples: C:\\Users\\name\\file.rs or D:/project/file.rs \
                 on Windows; /home/name/file.rs on Linux/macOS.",
                input.file_path.display()
            ),
            INVALID_INPUT_CODE,
        ));
    }

    for (index, edit) in input.edits.iter().enumerate() {
        if edit.old_string == edit.new_string {
            return Ok(ValidationOutcome::invalid(
                format!(
                    "edits[{index}]: no changes to make — `old_string` and `new_string` are identical"
                ),
                IDENTICAL_STRINGS_CODE,
            ));
        }
        if edit.old_string.is_empty() && index > 0 {
            return Ok(ValidationOutcome::invalid(
                format!(
                    "edits[{index}]: an empty `old_string` is only valid as the first edit of a \
                     new file"
                ),
                INVALID_INPUT_CODE,
            ));
        }
    }

    match fs::metadata(&input.file_path) {
        Ok(metadata) if !metadata.is_file() => Ok(ValidationOutcome::invalid(
            format!("Path is not a file: {}", input.file_path.display()),
            NOT_FILE_CODE,
        )),
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if input.creates_file() {
                Ok(ValidationOutcome::valid())
            } else {
                Ok(ValidationOutcome::invalid(
                    format!("File does not exist: {}", input.file_path.display()),
                    NOT_FOUND_CODE,
                ))
            }
        }
        Err(err) => Err(ToolError::Execution {
            tool: ToolId::new(FILE_MULTI_EDIT_TOOL_NAME),
            source: err.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{FILE_MODIFIED_SINCE_READ_CODE, MUST_READ_BEFORE_EDIT_CODE};
    use crate::tasks::test_support::TestConfigHome;
    use rebon_tools_core::file_state::FileStateCache;
    use rebon_tools_core::PermissionBehavior;
    use serde_json::json;

    struct TempDir {
        inner: tempfile::TempDir,
    }

    impl TempDir {
        fn new() -> Self {
            Self {
                inner: tempfile::Builder::new()
                    .prefix("rebon-multi-edit-tool-test-")
                    .tempdir()
                    .unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }
    }

    fn tool() -> MultiEditTool {
        MultiEditTool
    }

    fn edit(old: &str, new: &str) -> Value {
        json!({ "old_string": old, "new_string": new })
    }

    #[tokio::test]
    async fn applies_edits_in_order() {
        let dir = TempDir::new();
        let file = dir.path().join("demo.txt");
        fs::write(&file, "alpha beta gamma").unwrap();

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("alpha", "ALPHA"), edit("gamma", "GAMMA")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("update"));
        assert_eq!(out["editCount"], json!(2));
        assert_eq!(out["replacements"], json!(2));
        assert_eq!(fs::read_to_string(&file).unwrap(), "ALPHA beta GAMMA");
    }

    /// The defining difference from N separate Edit calls: edit 1 may
    /// match text edit 0 produced.
    #[tokio::test]
    async fn later_edit_sees_earlier_edit_result() {
        let dir = TempDir::new();
        let file = dir.path().join("chain.txt");
        fs::write(&file, "one").unwrap();

        tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("one", "two"), edit("two", "three")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "three");
    }

    #[tokio::test]
    async fn failing_edit_writes_nothing() {
        let dir = TempDir::new();
        let file = dir.path().join("atomic.txt");
        fs::write(&file, "alpha beta gamma").unwrap();

        let err = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [
                        edit("alpha", "ALPHA"),
                        edit("nonexistent", "X"),
                        edit("gamma", "GAMMA")
                    ]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(OLD_STRING_NOT_FOUND_CODE));
                assert!(
                    reason.contains("edits[1]"),
                    "should name the index: {reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // The successful edits[0] must not have survived.
        assert_eq!(fs::read_to_string(&file).unwrap(), "alpha beta gamma");
    }

    #[tokio::test]
    async fn ambiguous_edit_is_rejected_and_rolls_back() {
        let dir = TempDir::new();
        let file = dir.path().join("ambiguous.txt");
        fs::write(&file, "x head x").unwrap();

        let err = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("head", "HEAD"), edit("x", "y")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(AMBIGUOUS_MATCH_CODE));
                assert!(reason.contains("edits[1]"), "{reason}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), "x head x");
    }

    #[tokio::test]
    async fn replace_all_counts_every_occurrence() {
        let dir = TempDir::new();
        let file = dir.path().join("all.txt");
        fs::write(&file, "x head x").unwrap();

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [{ "old_string": "x", "new_string": "y", "replace_all": true }]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["replacements"], json!(2));
        assert_eq!(fs::read_to_string(&file).unwrap(), "y head y");
    }

    #[tokio::test]
    async fn identical_strings_are_rejected_before_any_write() {
        let dir = TempDir::new();
        let file = dir.path().join("same.txt");
        fs::write(&file, "alpha beta").unwrap();

        let result = tool()
            .validate_input(
                &json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("alpha", "ALPHA"), edit("beta", "beta")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.error_code, Some(IDENTICAL_STRINGS_CODE));
        assert!(result.message.unwrap().contains("edits[1]"));
    }

    #[tokio::test]
    async fn empty_edits_array_is_rejected() {
        let dir = TempDir::new();
        let file = dir.path().join("none.txt");
        fs::write(&file, "body").unwrap();

        let err = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "edits": [] }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { error_code, .. } => {
                assert_eq!(error_code, Some(NO_EDITS_CODE))
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn creates_file_from_empty_leading_old_string() {
        let dir = TempDir::new();
        let file = dir.path().join("new.txt");

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("", "line one\nline two\n"), edit("line two", "LINE TWO")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("create"));
        assert_eq!(out["originalFile"], Value::Null);
        assert_eq!(fs::read_to_string(&file).unwrap(), "line one\nLINE TWO\n");
    }

    #[tokio::test]
    async fn empty_old_string_after_the_first_edit_is_rejected() {
        let dir = TempDir::new();
        let file = dir.path().join("insert.txt");
        fs::write(&file, "body").unwrap();

        let result = tool()
            .validate_input(
                &json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("body", "BODY"), edit("", "appended")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert!(!result.is_valid());
        assert!(result.message.unwrap().contains("edits[1]"));
    }

    #[tokio::test]
    async fn matches_lf_old_string_against_crlf_file() {
        let dir = TempDir::new();
        let file = dir.path().join("crlf.txt");
        fs::write(&file, "line1\r\nline2\r\nline3\r\n").unwrap();

        tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [
                        edit("line1\nline2", "LINE1\nLINE2"),
                        edit("line3", "LINE3")
                    ]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "LINE1\r\nLINE2\r\nLINE3\r\n"
        );
    }

    #[tokio::test]
    async fn diff_span_narrows_to_the_changed_region() {
        let dir = TempDir::new();
        let file = dir.path().join("span.txt");
        let original = "a\nb\nc\nTARGET\ne\nf\ng\n";
        fs::write(&file, original).unwrap();

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("TARGET", "REPLACED")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        // The renderer locates oldString inside originalFile, so the
        // span must stay a substring of the original and must not drag
        // the untouched head/tail along.
        let old_string = out["oldString"].as_str().unwrap();
        assert_eq!(old_string, "TARGET\n");
        assert_eq!(out["newString"], json!("REPLACED\n"));
        assert!(original.contains(old_string));
    }

    #[tokio::test]
    async fn diff_span_survives_multibyte_content() {
        let dir = TempDir::new();
        let file = dir.path().join("utf8.txt");
        fs::write(&file, "序言\n目标：旧值\n结尾\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("旧值", "新值")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "序言\n目标：新值\n结尾\n"
        );
        let old_string = out["oldString"].as_str().unwrap();
        let new_string = out["newString"].as_str().unwrap();
        assert!(old_string.contains("旧值"), "{old_string:?}");
        assert!(new_string.contains("新值"), "{new_string:?}");
    }

    #[tokio::test]
    async fn requires_read_before_editing_an_unseen_file() {
        let dir = TempDir::new();
        let file = dir.path().join("unseen.txt");
        fs::write(&file, "hello world").unwrap();
        let ctx = ToolContext::new().with_file_state_cache(FileStateCache::new());

        let err = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("hello", "HELLO")]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { error_code, .. } => {
                assert_eq!(error_code, Some(MUST_READ_BEFORE_EDIT_CODE))
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn refreshes_cache_so_a_following_edit_needs_no_reread() {
        let dir = TempDir::new();
        let file = dir.path().join("cache.txt");
        fs::write(&file, "alpha beta").unwrap();
        let cache = FileStateCache::new();
        cache.set(
            &file,
            FileState {
                content: "alpha beta".to_string(),
                timestamp_ms: file_mtime_ms(&file).unwrap_or(0),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("alpha", "ALPHA")]
                }),
                &ctx,
            )
            .await
            .unwrap();

        let state = cache.get(&file).unwrap();
        assert_eq!(state.content, "ALPHA beta");
        assert_eq!(state.timestamp_ms, file_mtime_ms(&file).unwrap_or(0));

        // A second MultiEdit without an intervening Read must succeed.
        tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("beta", "BETA")]
                }),
                &ctx,
            )
            .await
            .expect("second multi-edit should not require another Read");
        assert_eq!(fs::read_to_string(&file).unwrap(), "ALPHA BETA");
    }

    #[tokio::test]
    async fn rejects_file_modified_since_read() {
        let dir = TempDir::new();
        let file = dir.path().join("drift.txt");
        fs::write(&file, "original content").unwrap();
        let cache = FileStateCache::new();
        cache.set(
            &file,
            FileState {
                content: "original content".to_string(),
                timestamp_ms: u64::MAX,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache);

        fs::write(&file, "someone else got here first").unwrap();

        let err = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("original", "ORIGINAL")]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { error_code, .. } => {
                assert_eq!(error_code, Some(FILE_MODIFIED_SINCE_READ_CODE))
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let err = tool()
            .call(
                json!({
                    "file_path": "relative/path.rs",
                    "edits": [edit("x", "y")]
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { error_code, .. } => {
                assert_eq!(error_code, Some(INVALID_INPUT_CODE))
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_paths_outside_subagent_write_scope() {
        let dir = TempDir::new();
        let inside_root = dir.path().join("scope");
        let outside = dir.path().join("elsewhere");
        fs::create_dir_all(&inside_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let target = outside.join("escape.txt");
        fs::write(&target, "hello world").unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("agent-scope")
            .with_cwd(inside_root.to_string_lossy().to_string())
            .with_path_scope_roots([inside_root]);

        let err = tool()
            .call(
                json!({
                    "file_path": target.to_string_lossy(),
                    "edits": [edit("hello", "bye")]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert!(
                    reason.contains("authorized roots"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn explicit_write_scope_rejects_project_edits() {
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let file = project.join("src.txt");
        fs::write(&file, "before").unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("verification")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);

        let result = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "edits": [edit("before", "after")]
                }),
                &ctx,
            )
            .await;

        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
        assert_eq!(fs::read_to_string(file).unwrap(), "before");
    }

    #[tokio::test]
    async fn explicit_write_scope_allows_scratchpad_without_prompt() {
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let file = scratchpad.join("result.txt");
        fs::write(&file, "before").unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("verification")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);
        let input = json!({
            "file_path": file.to_string_lossy(),
            "edits": [edit("before", "after")]
        });

        let decision = tool().check_permissions(&input, &ctx).await.unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Allow);
        tool().call(input, &ctx).await.unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "after");
    }

    #[tokio::test]
    async fn project_edits_ask_for_permission() {
        let dir = TempDir::new();
        let file = dir.path().join("src.txt");
        fs::write(&file, "before").unwrap();
        let input = json!({
            "file_path": file.to_string_lossy(),
            "edits": [edit("before", "after")]
        });

        let decision = tool()
            .check_permissions(&input, &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        let request = decision.request.unwrap();
        assert_eq!(request.title, "Edit file");
        assert!(request.message.contains("1 edit"), "{}", request.message);
    }

    #[tokio::test]
    async fn auto_memory_path_is_permission_allowed() {
        let config_home = TestConfigHome::new("multi-edit-memory-permission");
        let dir = TempDir::new();
        let cwd = dir.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let memory_file = rebon_session::memory_paths::repo_memory_dir(&cwd.to_string_lossy())
            .unwrap()
            .join("MEMORY.md");
        let input = json!({
            "file_path": memory_file.to_string_lossy(),
            "edits": [edit("before", "after")]
        });

        let decision = tool()
            .check_permissions(&input, &ToolContext::new().with_cwd(cwd.to_string_lossy()))
            .await
            .unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Allow);
        assert!(memory_file.starts_with(config_home.path()));
    }

    #[test]
    fn changed_span_returns_empty_for_identical_text() {
        assert_eq!(changed_span("same", "same"), (String::new(), String::new()));
    }

    #[test]
    fn changed_span_covers_scattered_edits_in_one_hunk() {
        let original = "head\nA\nmid\nB\ntail\n";
        let updated = "head\nX\nmid\nY\ntail\n";
        let (old, new) = changed_span(original, updated);
        assert!(original.contains(&old), "old span must be a substring");
        assert!(updated.contains(&new), "new span must be a substring");
        assert!(old.contains('A') && old.contains('B'), "{old:?}");
        assert!(new.contains('X') && new.contains('Y'), "{new:?}");
        assert!(!old.contains("head"), "leading context trimmed: {old:?}");
        assert!(!old.contains("tail"), "trailing context trimmed: {old:?}");
    }
}
