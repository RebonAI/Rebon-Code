//! `NotebookEdit` — replace, insert, or delete a cell in a Jupyter
//! `.ipynb` notebook.
//!
//! Notebooks are JSON, so a plain `Edit` against one means matching
//! escaped source strings inside a JSON blob — brittle, and it silently
//! produces invalid notebooks. This tool parses the document, mutates a
//! single cell, and re-serializes.
//!
//! ## Round-trip fidelity
//!
//! Everything the edit does not touch survives byte-for-byte in meaning:
//! notebook `metadata`, `nbformat` / `nbformat_minor`, and every other
//! cell including its `outputs` and `execution_count`.
//!
//! The serializer deliberately matches `nbformat`'s own writer — one
//! space of indent, keys sorted, trailing newline — because that is what
//! Jupyter produces, so a notebook edited here and then saved from
//! Jupyter shows no spurious diff. `serde_json::Map` is a `BTreeMap` in
//! this workspace, which sorts keys for free and happens to be exactly
//! nbformat's `sort_keys=True`.
//!
//! ## Cell addressing
//!
//! `Read` shows `.ipynb` files as raw JSON, so the `id` field of each
//! cell is directly visible to the model — that is the primary key. For
//! notebooks written before nbformat 4.5 (no per-cell `id`), a
//! `cell_id` that parses as a number is accepted as a 0-based index.

use async_trait::async_trait;
use rebon_tool::edit::{
    check_read_before_edit, check_read_before_edit_with_state, enforce_edit_path_scope,
    explicitly_authorized_write_path, is_auto_memory_path, normalize_path,
    FILE_MODIFIED_SINCE_READ_CODE, INVALID_INPUT_CODE, NOT_FILE_CODE, NOT_FOUND_CODE,
};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    file_state::{file_mtime_ms, FileState},
    require_valid_input, validation_outcome_from, PermissionDecision, PermissionRequest, ToolError,
    ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub use rebon_tool::NOTEBOOK_EDIT_TOOL_NAME;

/// The file is not valid JSON, or is JSON that is not a notebook.
const NOT_A_NOTEBOOK_CODE: i64 = 10;
/// `cell_id` did not resolve to a cell in this notebook.
const CELL_NOT_FOUND_CODE: i64 = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditMode {
    Replace,
    Insert,
    Delete,
}

impl EditMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "replace" => Some(Self::Replace),
            "insert" => Some(Self::Insert),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Insert => "insert",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone)]
struct NotebookEditInput {
    notebook_path: PathBuf,
    cell_id: Option<String>,
    new_source: String,
    cell_type: Option<String>,
    edit_mode: EditMode,
}

#[derive(Debug, Clone, Default)]
pub struct NotebookEditTool;

#[async_trait]
impl Tool for NotebookEditTool {
    fn id(&self) -> ToolId {
        ToolId::new(NOTEBOOK_EDIT_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["NotebookEditTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::FileEdit
    }

    fn file_target_field(&self) -> Option<&'static str> {
        Some("notebook_path")
    }

    /// Only .ipynb sessions need it; the schema stays out of every other
    /// session's prompt and the tool is reached through ToolSearch. The
    /// exposure policy reads this, so the decision lives with the tool.
    fn should_defer(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Replaces, inserts, or deletes a single cell in a Jupyter notebook (.ipynb).\n\
         \n\
         Usage:\n\
         - `notebook_path` must be an absolute path to an existing .ipynb file. Read it first; \
         this tool errors if you have not.\n\
         - `cell_id` is the cell's `id` field as it appears in the notebook JSON. For older \
         notebooks whose cells have no `id`, pass the 0-based cell index as a string instead.\n\
         - `edit_mode` is `replace` (default), `insert`, or `delete`.\n\
         - `replace`: `new_source` becomes the cell's source. Pass `cell_type` only to convert \
         the cell; converting to `code` gives it empty outputs, converting away from `code` \
         drops `outputs` and `execution_count`. A same-type replace leaves existing outputs \
         alone — they go stale, so re-run the cell.\n\
         - `insert`: a new cell is added AFTER `cell_id`, or at the top when `cell_id` is \
         omitted. `cell_type` is required.\n\
         - `delete`: removes the cell; `new_source` is ignored.\n\
         - Do not use `Edit` or `Write` on a .ipynb file — its source lives inside JSON string \
         escapes and hand-editing it produces a notebook Jupyter cannot open."
    }

    fn search_hint(&self) -> Option<&str> {
        Some("jupyter ipython python notebook cell")
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "The absolute path to the .ipynb file to edit"
                },
                "cell_id": {
                    "type": "string",
                    "description": "The `id` of the target cell as shown in the notebook JSON; a 0-based index is accepted for notebooks whose cells have no id. In insert mode the new cell goes after this one; omit to insert at the top."
                },
                "new_source": {
                    "type": "string",
                    "description": "The new cell source. Ignored in delete mode."
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "Required when inserting. In replace mode, pass it only to convert the cell's type."
                },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "description": "Defaults to replace."
                }
            },
            "required": ["notebook_path", "new_source"],
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
            Ok(parsed) => check_read_before_edit(&parsed.notebook_path, "", context),
            refused => validation_outcome_from(refused),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let path = input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");

        if explicitly_authorized_write_path(Path::new(path), context) {
            return Ok(PermissionDecision::allow(input.clone()));
        }

        if context.write_scope_roots().is_none() && is_auto_memory_path(Path::new(path), context) {
            return Ok(PermissionDecision::allow(input.clone()));
        }

        let mode = input
            .get("edit_mode")
            .and_then(Value::as_str)
            .unwrap_or("replace");
        let verb = match mode {
            "insert" => "insert a cell into",
            "delete" => "delete a cell from",
            _ => "modify a cell in",
        };
        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Edit notebook",
                format!("NotebookEdit wants to {verb}: {path}"),
            )
            .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = prepare_input(self.id(), &input, context)?;

        let observed_state = context
            .file_state_cache()
            .and_then(|cache| cache.get(&parsed.notebook_path));
        let _file_guard = rebon_tool::lock_file_for_write(&parsed.notebook_path).await;
        rebon_tool::ensure_file_not_mutated_in_current_batch(
            &parsed.notebook_path,
            context,
            self.id(),
            FILE_MODIFIED_SINCE_READ_CODE,
        )?;

        let state_check =
            check_read_before_edit_with_state(&parsed.notebook_path, "", context, observed_state)?;
        if !state_check.is_valid() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: state_check
                    .message
                    .unwrap_or_else(|| "NotebookEdit precondition failed".into()),
                error_code: state_check.error_code,
            });
        }

        let raw = match fs::read_to_string(&parsed.notebook_path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!(
                        "Notebook does not exist: {}. NotebookEdit edits existing notebooks; \
                         use Write to create one.",
                        parsed.notebook_path.display()
                    ),
                    error_code: Some(NOT_FOUND_CODE),
                });
            }
            Err(err) => {
                return Err(ToolError::Execution {
                    tool: self.id(),
                    source: err.into(),
                })
            }
        };

        let mut notebook: Value =
            serde_json::from_str(&raw).map_err(|err| ToolError::InvalidInput {
                tool: self.id(),
                reason: format!(
                    "{} is not valid JSON and cannot be edited as a notebook: {err}",
                    parsed.notebook_path.display()
                ),
                error_code: Some(NOT_A_NOTEBOOK_CODE),
            })?;

        let uses_crlf = raw.contains("\r\n");
        let trailing_newline = raw.ends_with('\n');
        let outcome = apply_notebook_edit(&self.id(), &mut notebook, &parsed)?;

        let serialized = serialize_notebook(&self.id(), &notebook, uses_crlf, trailing_newline)?;

        if !is_auto_memory_path(&parsed.notebook_path, context) {
            if let Some(tracker) = context.file_history_tracker() {
                tracker
                    .track_before_write(&parsed.notebook_path)
                    .map_err(|err| ToolError::Execution {
                        tool: self.id(),
                        source: err,
                    })?;
            }
        }

        fs::write(&parsed.notebook_path, serialized.as_bytes()).map_err(|err| {
            ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            }
        })?;
        rebon_tool::record_file_mutation_in_current_batch(&parsed.notebook_path, context);

        // Keep the file-state cache honest: the model's Read snapshot of
        // this notebook is now stale, and refreshing it here is what lets
        // a follow-up NotebookEdit run without another Read.
        if let Some(cache) = context.file_state_cache() {
            let timestamp_ms = file_mtime_ms(&parsed.notebook_path).unwrap_or(0);
            cache.set(
                &parsed.notebook_path,
                FileState {
                    content: serialized.clone(),
                    timestamp_ms,
                    offset: None,
                    limit: None,
                    is_partial_view: false,
                },
            );
        }

        // `filePath` + `oldString` + `newString` is the shape the shared
        // renderer turns into a diff card. Scoped to the cell source, so
        // the card shows the cell that changed rather than a JSON blob.
        // `originalFile` is deliberately absent — the surrounding context
        // in the file is escaped JSON, not something worth showing.
        Ok(json!({
            "type": "update",
            "filePath": normalize_path(&parsed.notebook_path),
            "cellId": outcome.cell_id,
            "cellIndex": outcome.cell_index,
            "cellType": outcome.cell_type,
            "editMode": parsed.edit_mode.as_str(),
            "oldString": outcome.old_source,
            "newString": outcome.new_source,
            "cellCount": outcome.cell_count,
        }))
    }
}

struct EditOutcome {
    cell_id: Option<String>,
    cell_index: usize,
    cell_type: String,
    old_source: String,
    new_source: String,
    cell_count: usize,
}

fn apply_notebook_edit(
    tool: &ToolId,
    notebook: &mut Value,
    input: &NotebookEditInput,
) -> ToolResult<EditOutcome> {
    let notebook_object = notebook
        .as_object_mut()
        .ok_or_else(|| not_a_notebook(tool, "the top-level JSON value is not an object"))?;
    let minor = notebook_object
        .get("nbformat_minor")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cells = notebook_object
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| not_a_notebook(tool, "it has no top-level `cells` array"))?;

    let existing_ids = cells.iter().any(|cell| cell.get("id").is_some());

    match input.edit_mode {
        EditMode::Insert => {
            let cell_type = input.cell_type.as_deref().ok_or_else(|| {
                ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "`cell_type` is required when `edit_mode` is `insert` — a new cell has no type to inherit".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                }
            })?;
            // `cell_id` names the cell to insert *after*; no id means
            // "insert at the top".
            let index = match input.cell_id.as_deref() {
                Some(cell_id) => resolve_cell_index(tool, cells, cell_id)? + 1,
                None => 0,
            };
            let mut cell = Map::new();
            cell.insert("cell_type".into(), Value::String(cell_type.to_string()));
            if existing_ids || minor >= 5 {
                cell.insert("id".into(), Value::String(generate_cell_id()));
            }
            cell.insert("metadata".into(), Value::Object(Map::new()));
            cell.insert("source".into(), source_value(&input.new_source, true));
            if cell_type == "code" {
                cell.insert("execution_count".into(), Value::Null);
                cell.insert("outputs".into(), Value::Array(Vec::new()));
            }
            let cell_id = cell.get("id").and_then(Value::as_str).map(str::to_string);
            cells.insert(index, Value::Object(cell));
            Ok(EditOutcome {
                cell_id,
                cell_index: index,
                cell_type: cell_type.to_string(),
                old_source: String::new(),
                new_source: input.new_source.clone(),
                cell_count: cells.len(),
            })
        }
        EditMode::Delete => {
            let cell_id = input
                .cell_id
                .as_deref()
                .ok_or_else(|| ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "`cell_id` is required when `edit_mode` is `delete`".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                })?;
            let index = resolve_cell_index(tool, cells, cell_id)?;
            let removed = cells.remove(index);
            Ok(EditOutcome {
                cell_id: removed
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                cell_index: index,
                cell_type: removed
                    .get("cell_type")
                    .and_then(Value::as_str)
                    .unwrap_or("code")
                    .to_string(),
                old_source: read_source(&removed),
                new_source: String::new(),
                cell_count: cells.len(),
            })
        }
        EditMode::Replace => {
            let cell_id = input.cell_id.as_deref().ok_or_else(|| {
                ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "`cell_id` is required when `edit_mode` is `replace` — pass the target cell's `id`, or its 0-based index for notebooks whose cells have no id".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                }
            })?;
            let index = resolve_cell_index(tool, cells, cell_id)?;
            let cell = cells[index]
                .as_object_mut()
                .ok_or_else(|| not_a_notebook(tool, "a cell is not a JSON object"))?;

            let old_source = read_source(&Value::Object(cell.clone()));
            let previous_type = cell
                .get("cell_type")
                .and_then(Value::as_str)
                .unwrap_or("code")
                .to_string();
            // Preserve however this cell already spells its source —
            // nbformat allows a plain string or a list of lines, and
            // flipping the representation is pure diff noise.
            let as_list = cell
                .get("source")
                .map(|source| source.is_array())
                .unwrap_or(true);
            cell.insert("source".into(), source_value(&input.new_source, as_list));

            let cell_type = match input.cell_type.as_deref() {
                Some(requested) if requested != previous_type => {
                    cell.insert("cell_type".into(), Value::String(requested.to_string()));
                    // The code-only fields are structurally invalid on a
                    // markdown cell and meaningless on a freshly
                    // converted code cell.
                    if requested == "code" {
                        cell.insert("execution_count".into(), Value::Null);
                        cell.insert("outputs".into(), Value::Array(Vec::new()));
                    } else {
                        cell.remove("execution_count");
                        cell.remove("outputs");
                    }
                    requested.to_string()
                }
                _ => previous_type,
            };

            let cell_id = cell.get("id").and_then(Value::as_str).map(str::to_string);
            Ok(EditOutcome {
                cell_id,
                cell_index: index,
                cell_type,
                old_source,
                new_source: input.new_source.clone(),
                cell_count: cells.len(),
            })
        }
    }
}

fn not_a_notebook(tool: &ToolId, detail: &str) -> ToolError {
    ToolError::InvalidInput {
        tool: tool.clone(),
        reason: format!("This file is not a Jupyter notebook: {detail}."),
        error_code: Some(NOT_A_NOTEBOOK_CODE),
    }
}

/// Resolve `cell_id` to a position: the `id` field first, then a
/// 0-based index for notebooks predating nbformat 4.5.
fn resolve_cell_index(tool: &ToolId, cells: &[Value], cell_id: &str) -> ToolResult<usize> {
    if let Some(index) = cells
        .iter()
        .position(|cell| cell.get("id").and_then(Value::as_str) == Some(cell_id))
    {
        return Ok(index);
    }
    if let Ok(index) = cell_id.parse::<usize>() {
        if index < cells.len() {
            return Ok(index);
        }
    }
    Err(ToolError::InvalidInput {
        tool: tool.clone(),
        reason: format!(
            "No cell with id `{cell_id}` in this notebook ({} cells). Re-read the notebook and \
             use a cell's `id` field, or a 0-based index if its cells have no ids.",
            cells.len()
        ),
        error_code: Some(CELL_NOT_FOUND_CODE),
    })
}

/// Read a cell's source, joining the list-of-lines form nbformat also
/// permits.
fn read_source(cell: &Value) -> String {
    match cell.get("source") {
        Some(Value::String(source)) => source.clone(),
        Some(Value::Array(lines)) => lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

/// Render `source` in the requested nbformat representation. The list
/// form keeps the newline on the end of each line except the last,
/// which is how Jupyter writes it.
fn source_value(source: &str, as_list: bool) -> Value {
    if !as_list {
        return Value::String(source.to_string());
    }
    if source.is_empty() {
        return Value::Array(Vec::new());
    }
    let mut lines = Vec::new();
    let mut rest = source;
    while let Some(index) = rest.find('\n') {
        lines.push(Value::String(rest[..=index].to_string()));
        rest = &rest[index + 1..];
    }
    if !rest.is_empty() {
        lines.push(Value::String(rest.to_string()));
    }
    Value::Array(lines)
}

/// Serialize with `nbformat`'s own conventions so a notebook this tool
/// touched and one Jupyter saved are byte-identical: one space of
/// indent, sorted keys (free — `serde_json::Map` is a `BTreeMap` here),
/// and a trailing newline.
fn serialize_notebook(
    tool: &ToolId,
    notebook: &Value,
    uses_crlf: bool,
    trailing_newline: bool,
) -> ToolResult<String> {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    serde::Serialize::serialize(notebook, &mut serializer).map_err(|err| ToolError::Execution {
        tool: tool.clone(),
        source: err.into(),
    })?;
    let mut serialized = String::from_utf8(buffer).map_err(|err| ToolError::Execution {
        tool: tool.clone(),
        source: err.into(),
    })?;
    if trailing_newline {
        serialized.push('\n');
    }
    if uses_crlf {
        serialized = serialized.replace('\n', "\r\n");
    }
    Ok(serialized)
}

fn generate_cell_id() -> String {
    let mut buf = [0u8; 4];
    if getrandom::getrandom(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf.copy_from_slice(&(nanos as u32).to_le_bytes());
    }
    format!("{:08x}", u32::from_le_bytes(buf))
}

/// Parse and vet one `NotebookEdit` request, once.
///
/// Both entry points ask the same three questions in the same order — does
/// the input parse, does the path clear the edit scope, is the parsed request
/// well-formed. They part company afterwards: `validate_input` answers the
/// read-before-edit question from the cache, `call` answers it against the
/// state it is about to write.
fn prepare_input(
    tool: ToolId,
    input: &Value,
    context: &ToolContext,
) -> ToolResult<NotebookEditInput> {
    let parsed = parse_input(input)?;
    enforce_edit_path_scope(tool.clone(), &parsed.notebook_path, context)?;
    require_valid_input(
        tool,
        validate_parsed_input(&parsed)?,
        "NotebookEdit input is invalid",
    )?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<NotebookEditInput> {
    let tool = ToolId::new(NOTEBOOK_EDIT_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "NotebookEdit input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let notebook_path = object
        .get("notebook_path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "NotebookEdit input requires a string `notebook_path`".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;

    let edit_mode = match object.get("edit_mode") {
        Some(Value::String(raw)) => {
            EditMode::parse(raw).ok_or_else(|| ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!(
                    "`edit_mode` must be `replace`, `insert`, or `delete`, got `{raw}`"
                ),
                error_code: Some(INVALID_INPUT_CODE),
            })?
        }
        Some(Value::Null) | None => EditMode::Replace,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`edit_mode` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    // Delete throws its source away, so requiring it there would just
    // make the model invent a value.
    let new_source = match object.get("new_source") {
        Some(Value::String(source)) => source.clone(),
        Some(Value::Null) | None if edit_mode == EditMode::Delete => String::new(),
        _ => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "NotebookEdit input requires a string `new_source`".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    let cell_type = match object.get("cell_type") {
        Some(Value::String(raw)) if raw == "code" || raw == "markdown" => Some(raw.clone()),
        Some(Value::String(raw)) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: format!("`cell_type` must be `code` or `markdown`, got `{raw}`"),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`cell_type` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    let cell_id = match object.get("cell_id") {
        Some(Value::String(raw)) if !raw.is_empty() => Some(raw.clone()),
        Some(Value::String(_)) | Some(Value::Null) | None => None,
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`cell_id` must be a string when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
    };

    Ok(NotebookEditInput {
        notebook_path: PathBuf::from(notebook_path),
        cell_id,
        new_source,
        cell_type,
        edit_mode,
    })
}

fn validate_parsed_input(input: &NotebookEditInput) -> ToolResult<ValidationOutcome> {
    if !input.notebook_path.is_absolute() {
        return Ok(ValidationOutcome::invalid(
            format!(
                "NotebookEdit requires an absolute `notebook_path`, got: {}. \
                 Examples: C:\\Users\\name\\analysis.ipynb or D:/project/analysis.ipynb \
                 on Windows; /home/name/analysis.ipynb on Linux/macOS.",
                input.notebook_path.display()
            ),
            INVALID_INPUT_CODE,
        ));
    }

    match fs::metadata(&input.notebook_path) {
        Ok(metadata) if !metadata.is_file() => Ok(ValidationOutcome::invalid(
            format!("Path is not a file: {}", input.notebook_path.display()),
            NOT_FILE_CODE,
        )),
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ValidationOutcome::invalid(
            format!("Notebook does not exist: {}", input.notebook_path.display()),
            NOT_FOUND_CODE,
        )),
        Err(err) => Err(ToolError::Execution {
            tool: ToolId::new(NOTEBOOK_EDIT_TOOL_NAME),
            source: err.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_agent_core::file_history::FileHistoryTracker;
    use rebon_tool::edit::MUST_READ_BEFORE_EDIT_CODE;
    use rebon_tools_core::file_state::FileStateCache;
    use rebon_tools_core::PermissionBehavior;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct TempDir {
        inner: tempfile::TempDir,
    }

    impl TempDir {
        fn new() -> Self {
            Self {
                inner: tempfile::Builder::new()
                    .prefix("rebon-notebook-edit-tool-test-")
                    .tempdir()
                    .unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }
    }

    struct BlockingHistoryTracker {
        entered: Arc<tokio::sync::Notify>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl FileHistoryTracker for BlockingHistoryTracker {
        fn track_before_write(&self, _file_path: &Path) -> anyhow::Result<()> {
            self.entered.notify_one();
            self.release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("test release signal");
            Ok(())
        }
    }

    struct ProbeHistoryTracker {
        entered: Arc<AtomicBool>,
    }

    impl FileHistoryTracker for ProbeHistoryTracker {
        fn track_before_write(&self, _file_path: &Path) -> anyhow::Result<()> {
            self.entered.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn observed_context(
        path: &Path,
        content: &str,
        tracker: Option<Arc<dyn FileHistoryTracker>>,
    ) -> ToolContext {
        let cache = FileStateCache::new();
        cache.set(
            path,
            FileState {
                content: content.to_string(),
                timestamp_ms: file_mtime_ms(path).unwrap_or(0),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let context = ToolContext::new().with_file_state_cache(cache);
        match tracker {
            Some(tracker) => context.with_file_history_tracker(tracker),
            None => context,
        }
    }

    fn tool() -> NotebookEditTool {
        NotebookEditTool
    }

    /// A minimal but realistic nbformat 4.5 notebook: two cells with
    /// ids, a code cell carrying outputs and an execution count, plus
    /// notebook-level metadata that must survive every edit.
    fn notebook_json() -> Value {
        json!({
            "cells": [
                {
                    "cell_type": "markdown",
                    "id": "intro",
                    "metadata": {},
                    "source": ["# Title\n", "\n", "Some prose.\n"]
                },
                {
                    "cell_type": "code",
                    "execution_count": 7,
                    "id": "compute",
                    "metadata": { "tags": ["slow"] },
                    "outputs": [
                        { "name": "stdout", "output_type": "stream", "text": ["42\n"] }
                    ],
                    "source": ["print(6 * 7)\n"]
                }
            ],
            "metadata": {
                "kernelspec": { "display_name": "Python 3", "language": "python", "name": "python3" },
                "language_info": { "name": "python", "version": "3.11.4" }
            },
            "nbformat": 4,
            "nbformat_minor": 5
        })
    }

    fn write_notebook(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("demo.ipynb");
        let mut text = serde_json::to_string_pretty(&notebook_json()).unwrap();
        text.push('\n');
        fs::write(&path, text).unwrap();
        path
    }

    fn read_notebook(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn replace_updates_source_and_keeps_everything_else() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        let out = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "print('replaced')\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["editMode"], json!("replace"));
        assert_eq!(out["cellId"], json!("compute"));
        assert_eq!(out["cellIndex"], json!(1));
        assert_eq!(out["cellType"], json!("code"));
        assert_eq!(out["oldString"], json!("print(6 * 7)\n"));
        assert_eq!(out["newString"], json!("print('replaced')\n"));

        let after = read_notebook(&path);
        let cell = &after["cells"][1];
        assert_eq!(cell["source"], json!(["print('replaced')\n"]));
        // Untouched structure survives.
        assert_eq!(cell["execution_count"], json!(7));
        assert_eq!(cell["metadata"]["tags"], json!(["slow"]));
        assert_eq!(cell["outputs"][0]["text"], json!(["42\n"]));
        assert_eq!(after["cells"][0], notebook_json()["cells"][0]);
        assert_eq!(after["metadata"], notebook_json()["metadata"]);
        assert_eq!(after["nbformat"], json!(4));
        assert_eq!(after["nbformat_minor"], json!(5));
    }

    #[tokio::test]
    async fn replace_can_address_a_cell_by_index() {
        let dir = TempDir::new();
        let path = dir.path().join("legacy.ipynb");
        // nbformat 4.4: cells have no `id`, so index addressing is the
        // only way in.
        fs::write(
            &path,
            serde_json::to_string(&json!({
                "cells": [
                    { "cell_type": "code", "execution_count": null, "metadata": {}, "outputs": [], "source": ["old\n"] }
                ],
                "metadata": {},
                "nbformat": 4,
                "nbformat_minor": 4
            }))
            .unwrap(),
        )
        .unwrap();

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "0",
                    "new_source": "new\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(read_notebook(&path)["cells"][0]["source"], json!(["new\n"]));
    }

    #[tokio::test]
    async fn replace_preserves_a_plain_string_source() {
        let dir = TempDir::new();
        let path = dir.path().join("string-source.ipynb");
        fs::write(
            &path,
            serde_json::to_string(&json!({
                "cells": [
                    { "cell_type": "code", "id": "c1", "metadata": {}, "outputs": [], "source": "old\n" }
                ],
                "metadata": {},
                "nbformat": 4,
                "nbformat_minor": 5
            }))
            .unwrap(),
        )
        .unwrap();

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "c1",
                    "new_source": "one\ntwo\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            read_notebook(&path)["cells"][0]["source"],
            json!("one\ntwo\n"),
            "a string source must not be rewritten as a list"
        );
    }

    #[tokio::test]
    async fn converting_to_markdown_drops_code_only_fields() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "## Notes\n",
                    "cell_type": "markdown"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let cell = read_notebook(&path)["cells"][1].clone();
        assert_eq!(cell["cell_type"], json!("markdown"));
        assert!(cell.get("outputs").is_none(), "{cell}");
        assert!(cell.get("execution_count").is_none(), "{cell}");
    }

    #[tokio::test]
    async fn converting_to_code_gives_empty_outputs() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "intro",
                    "new_source": "x = 1\n",
                    "cell_type": "code"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let cell = read_notebook(&path)["cells"][0].clone();
        assert_eq!(cell["cell_type"], json!("code"));
        assert_eq!(cell["outputs"], json!([]));
        assert_eq!(cell["execution_count"], Value::Null);
    }

    #[tokio::test]
    async fn insert_places_the_new_cell_after_the_named_one() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        let out = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "intro",
                    "new_source": "import os\n",
                    "cell_type": "code",
                    "edit_mode": "insert"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["cellIndex"], json!(1));
        assert_eq!(out["cellCount"], json!(3));
        let after = read_notebook(&path);
        let inserted = &after["cells"][1];
        assert_eq!(inserted["cell_type"], json!("code"));
        assert_eq!(inserted["source"], json!(["import os\n"]));
        assert_eq!(inserted["outputs"], json!([]));
        assert_eq!(inserted["execution_count"], Value::Null);
        assert_eq!(inserted["metadata"], json!({}));
        assert!(
            inserted["id"].as_str().is_some_and(|id| !id.is_empty()),
            "a 4.5 notebook needs an id on every cell: {inserted}"
        );
        // The displaced cell keeps its identity and outputs.
        assert_eq!(after["cells"][2]["id"], json!("compute"));
        assert_eq!(after["cells"][2]["execution_count"], json!(7));
    }

    #[tokio::test]
    async fn insert_without_cell_id_goes_to_the_top() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        let out = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "new_source": "# Header\n",
                    "cell_type": "markdown",
                    "edit_mode": "insert"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["cellIndex"], json!(0));
        let after = read_notebook(&path);
        assert_eq!(after["cells"][0]["source"], json!(["# Header\n"]));
        assert_eq!(after["cells"][0]["cell_type"], json!("markdown"));
        // A markdown cell must not carry code-only fields.
        assert!(after["cells"][0].get("outputs").is_none());
        assert_eq!(after["cells"][1]["id"], json!("intro"));
    }

    #[tokio::test]
    async fn insert_requires_a_cell_type() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        let err = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "new_source": "x = 1\n",
                    "edit_mode": "insert"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert!(reason.contains("cell_type"), "{reason}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert_eq!(read_notebook(&path)["cells"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn delete_removes_the_cell_and_leaves_the_rest() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        let out = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "intro",
                    "new_source": "",
                    "edit_mode": "delete"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["cellCount"], json!(1));
        assert_eq!(out["oldString"], json!("# Title\n\nSome prose.\n"));
        assert_eq!(out["newString"], json!(""));
        let after = read_notebook(&path);
        assert_eq!(after["cells"].as_array().unwrap().len(), 1);
        assert_eq!(after["cells"][0]["id"], json!("compute"));
        assert_eq!(after["metadata"], notebook_json()["metadata"]);
    }

    #[tokio::test]
    async fn delete_does_not_require_new_source() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "intro",
                    "edit_mode": "delete"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(read_notebook(&path)["cells"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_cell_id_is_rejected_without_writing() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);
        let before = fs::read_to_string(&path).unwrap();

        let err = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "nope",
                    "new_source": "x\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { error_code, .. } => {
                assert_eq!(error_code, Some(CELL_NOT_FOUND_CODE))
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn invalid_json_is_rejected_with_a_clear_error() {
        let dir = TempDir::new();
        let path = dir.path().join("broken.ipynb");
        fs::write(&path, "{ not json").unwrap();

        let err = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "0",
                    "new_source": "x\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(NOT_A_NOTEBOOK_CODE));
                assert!(reason.contains("not valid JSON"), "{reason}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn valid_json_that_is_not_a_notebook_is_rejected() {
        let dir = TempDir::new();
        let path = dir.path().join("config.ipynb");
        fs::write(&path, r#"{"hello": "world"}"#).unwrap();

        let err = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "0",
                    "new_source": "x\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(NOT_A_NOTEBOOK_CODE));
                assert!(reason.contains("cells"), "{reason}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_notebook_is_rejected() {
        let dir = TempDir::new();
        let path = dir.path().join("absent.ipynb");

        let err = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "0",
                    "new_source": "x\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { error_code, .. } => {
                assert_eq!(error_code, Some(NOT_FOUND_CODE))
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serialized_output_matches_nbformat_conventions() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "print('x')\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("\n"), "nbformat writes a trailing newline");
        assert!(
            text.contains("\n \"cells\": ["),
            "nbformat indents with a single space: {}",
            &text[..text.len().min(120)]
        );
        // Round-trips as a notebook.
        assert_eq!(read_notebook(&path)["nbformat"], json!(4));
    }

    #[tokio::test]
    async fn crlf_notebook_is_written_back_with_crlf() {
        let dir = TempDir::new();
        let path = dir.path().join("crlf.ipynb");
        let mut text = serde_json::to_string_pretty(&notebook_json()).unwrap();
        text.push('\n');
        fs::write(&path, text.replace('\n', "\r\n")).unwrap();

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "print('x')\n"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("\r\n"), "CRLF style must survive");
        assert!(
            !after.replace("\r\n", "").contains('\n'),
            "no bare LF should remain in the serialized JSON"
        );
        // The embedded source newline is JSON-escaped, so it is not
        // affected by the line-ending conversion.
        assert_eq!(
            read_notebook(&path)["cells"][1]["source"],
            json!(["print('x')\n"])
        );
    }

    #[tokio::test]
    async fn requires_read_before_edit() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);
        let ctx = ToolContext::new().with_file_state_cache(FileStateCache::new());

        let err = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "x\n"
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
    }

    #[tokio::test]
    async fn refreshes_cache_so_a_second_edit_needs_no_reread() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);
        let cache = FileStateCache::new();
        cache.set(
            &path,
            FileState {
                content: fs::read_to_string(&path).unwrap(),
                timestamp_ms: file_mtime_ms(&path).unwrap_or(0),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache);

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "first\n"
                }),
                &ctx,
            )
            .await
            .unwrap();
        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "second\n"
                }),
                &ctx,
            )
            .await
            .expect("second notebook edit should not require another Read");

        assert_eq!(
            read_notebook(&path)["cells"][1]["source"],
            json!(["second\n"])
        );
    }

    #[tokio::test]
    async fn same_dispatch_batch_rejects_second_notebook_edit() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);
        let initial = fs::read_to_string(&path).unwrap();
        let context = observed_context(&path, &initial, None).with_fresh_file_mutation_batch();

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "first\n"
                }),
                &context,
            )
            .await
            .expect("first notebook edit in the batch");
        let error = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "second\n"
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ToolError::InvalidInput {
                error_code: Some(FILE_MODIFIED_SINCE_READ_CODE),
                ..
            }
        ));
        assert_eq!(
            read_notebook(&path)["cells"][1]["source"],
            json!(["first\n"])
        );

        tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "second\n"
                }),
                &context.clone().with_fresh_file_mutation_batch(),
            )
            .await
            .expect("a later batch may build on the refreshed notebook cache");
        assert_eq!(
            read_notebook(&path)["cells"][1]["source"],
            json!(["second\n"])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_notebook_edits_serialize_and_reject_the_stale_writer() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);
        let initial = fs::read_to_string(&path).unwrap();
        let first_entered = Arc::new(tokio::sync::Notify::new());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_context = observed_context(
            &path,
            &initial,
            Some(Arc::new(BlockingHistoryTracker {
                entered: first_entered.clone(),
                release: Mutex::new(release_rx),
            })),
        );
        let second_entered = Arc::new(AtomicBool::new(false));
        let second_context = observed_context(
            &path,
            &initial,
            Some(Arc::new(ProbeHistoryTracker {
                entered: second_entered.clone(),
            })),
        );
        let first_path = path.clone();
        let first = tokio::spawn(async move {
            tool()
                .call(
                    json!({
                        "notebook_path": first_path.to_string_lossy(),
                        "cell_id": "compute",
                        "new_source": "first\n"
                    }),
                    &first_context,
                )
                .await
        });
        first_entered.notified().await;

        let second_path = path.clone();
        let second_started = Arc::new(tokio::sync::Notify::new());
        let second_started_task = second_started.clone();
        let second = tokio::spawn(async move {
            second_started_task.notify_one();
            tool()
                .call(
                    json!({
                        "notebook_path": second_path.to_string_lossy(),
                        "cell_id": "compute",
                        "new_source": "second\n"
                    }),
                    &second_context,
                )
                .await
        });
        second_started.notified().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            !second_entered.load(Ordering::SeqCst),
            "the second NotebookEdit reached file history while the first held the path lock"
        );

        release_tx.send(()).unwrap();
        first.await.unwrap().expect("first notebook edit");
        let error = second.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            ToolError::InvalidInput {
                error_code: Some(FILE_MODIFIED_SINCE_READ_CODE),
                ..
            }
        ));
        assert_eq!(
            read_notebook(&path)["cells"][1]["source"],
            json!(["first\n"])
        );
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let err = tool()
            .call(
                json!({ "notebook_path": "relative/demo.ipynb", "cell_id": "0", "new_source": "x" }),
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
        let path = outside.join("escape.ipynb");
        let mut text = serde_json::to_string_pretty(&notebook_json()).unwrap();
        text.push('\n');
        fs::write(&path, &text).unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("agent-scope")
            .with_cwd(inside_root.to_string_lossy().to_string())
            .with_path_scope_roots([inside_root]);

        let err = tool()
            .call(
                json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "compute",
                    "new_source": "x\n"
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
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[tokio::test]
    async fn project_notebooks_ask_for_permission() {
        let dir = TempDir::new();
        let path = write_notebook(&dir);
        let input = json!({
            "notebook_path": path.to_string_lossy(),
            "cell_id": "compute",
            "new_source": "x\n"
        });

        let decision = tool()
            .check_permissions(&input, &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Ask);
        assert_eq!(decision.request.unwrap().title, "Edit notebook");
    }

    #[tokio::test]
    async fn explicit_write_scope_allows_scratchpad_without_prompt() {
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let path = scratchpad.join("demo.ipynb");
        let mut text = serde_json::to_string_pretty(&notebook_json()).unwrap();
        text.push('\n');
        fs::write(&path, text).unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("verification")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);
        let input = json!({
            "notebook_path": path.to_string_lossy(),
            "cell_id": "compute",
            "new_source": "scoped\n"
        });

        let decision = tool().check_permissions(&input, &ctx).await.unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Allow);
        tool().call(input, &ctx).await.unwrap();
        assert_eq!(
            read_notebook(&path)["cells"][1]["source"],
            json!(["scoped\n"])
        );
    }

    #[test]
    fn source_value_splits_lines_keeping_terminators() {
        assert_eq!(
            source_value("a\nb\n", true),
            json!(["a\n", "b\n"]),
            "each line keeps its newline"
        );
        assert_eq!(
            source_value("a\nb", true),
            json!(["a\n", "b"]),
            "a final line without a newline stays unterminated"
        );
        assert_eq!(source_value("", true), json!([]));
        assert_eq!(source_value("a\nb", false), json!("a\nb"));
    }

    #[test]
    fn read_source_joins_both_nbformat_representations() {
        assert_eq!(read_source(&json!({ "source": ["a\n", "b"] })), "a\nb");
        assert_eq!(read_source(&json!({ "source": "a\nb" })), "a\nb");
        assert_eq!(read_source(&json!({})), "");
    }
}
