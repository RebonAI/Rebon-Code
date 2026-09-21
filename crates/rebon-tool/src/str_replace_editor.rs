//! `str_replace_editor` — the editor half of the Anchored Minimal bootstrap pair.
//!
//! The model-facing name, description, and parameter schema are copied from
//! DeepSeek Harness' `@deepseek-ai/dsh-tool-str-replace-editor` so the first
//! request of an anchored session carries the exact tool identity the upstream
//! measurement depends on. That text is MIT-licensed; the notice lives in
//! `runtimes/node/plugins/deepseek-responses/NOTICE`.
//!
//! Execution is a translation layer, not a second editor: `view` delegates to
//! [`ReadTool`], `create` and `insert` to [`WriteTool`], and `str_replace` to
//! [`EditTool`], so permissions, the read-before-write file-state cache, and
//! path scoping behave exactly as they do for the native tools.

use crate::edit::EditTool;
use crate::read::ReadTool;
use crate::write::WriteTool;
use crate::{Tool, ToolContext};
use async_trait::async_trait;
use rebon_tools_core::{
    validation_outcome_from, PermissionBehavior, PermissionDecision, ToolError, ToolId,
    ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const STR_REPLACE_EDITOR_TOOL_NAME: &str = "str_replace_editor";
const INVALID_INPUT_CODE: i64 = 400;
/// Directory listing depth, matching the upstream tool's documented behavior.
const LIST_DEPTH: usize = 2;

/// Verbatim upstream description (MIT, DeepSeek Harness).
pub const STR_REPLACE_EDITOR_DESCRIPTION: &str = "Custom editing tool for viewing, creating and editing files\n\
* State is persistent across command calls and discussions with the user\n\
* If `path` is a file, `view` displays the result of applying `cat -n`. If `path` is a directory, `view` lists non-hidden files and directories up to 2 levels deep\n\
* The `create` command cannot be used if the specified `path` already exists as a file\n\
* If a `command` generates a long output, it will be truncated and marked with `<response clipped>`\n\
\n\
Notes for using the `str_replace` command:\n\
* The `old_str` parameter should match EXACTLY one or more consecutive lines from the original file. Be mindful of whitespaces!\n\
* If the `old_str` parameter is not unique in the file, the replacement will not be performed. Make sure to include enough context in `old_str` to make it unique\n\
* The `new_str` parameter should contain the edited lines that should replace the `old_str`";

/// Verbatim upstream parameter schema (MIT, DeepSeek Harness). Property order
/// and the absent `additionalProperties` are part of the anchored identity.
pub fn str_replace_editor_input_schema() -> ToolInputSchema {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["view", "create", "str_replace", "insert"],
                "description": "The commands to run. Allowed options are: `view`, `create`, `str_replace`, `insert`."
            },
            "path": {
                "type": "string",
                "description": "Absolute path to file or directory, e.g. `/repo/file.py` or `/repo`."
            },
            "file_text": {
                "type": "string",
                "description": "Required parameter of `create` command, with the content of the file to be created."
            },
            "insert_line": {
                "type": "integer",
                "description": "Required parameter of `insert` command. The `new_str` will be inserted AFTER the line `insert_line` of `path`."
            },
            "new_str": {
                "type": "string",
                "description": "Optional parameter of `str_replace` command containing the new string (if not given, no string will be added). Required parameter of `insert` command containing the string to insert."
            },
            "old_str": {
                "type": "string",
                "description": "Required parameter of `str_replace` command containing the string in `path` to replace."
            },
            "view_range": {
                "type": "array",
                "items": { "type": "integer" },
                "description": "Optional parameter of `view` command when `path` points to a file. If none is given, the full file is shown. If provided, the file will be shown in the indicated line number range, e.g. [11, 12] will show lines 11 and 12. Indexing at 1 to start. Setting `[start_line, -1]` shows all lines from `start_line` to the end of the file."
            }
        },
        "required": ["command", "path"]
    })
}

#[derive(Debug, Clone, Default)]
pub struct StrReplaceEditorTool;

/// Which native tool executes a translated call, if any.
#[derive(Debug)]
enum Target {
    Read,
    Write,
    Edit,
    /// `view` on a directory has no native counterpart and is served inline.
    ListDirectory(PathBuf),
}

#[derive(Debug)]
struct Translated {
    target: Target,
    /// Input shaped for the target tool. For `insert` during validation this
    /// carries an empty body: the spliced content is only computed at call
    /// time, and neither `Write::validate_input` nor its permission shaping
    /// inspects `content`.
    input: Value,
}

impl Translated {
    fn tool(&self) -> Option<Box<dyn Tool>> {
        match self.target {
            Target::Read => Some(Box::new(ReadTool)),
            Target::Write => Some(Box::new(WriteTool)),
            Target::Edit => Some(Box::new(EditTool)),
            Target::ListDirectory(_) => None,
        }
    }
}

fn invalid(reason: impl Into<String>) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(STR_REPLACE_EDITOR_TOOL_NAME),
        reason: reason.into(),
        error_code: Some(INVALID_INPUT_CODE),
    }
}

fn string_field(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn required_field(input: &Value, key: &str, command: &str) -> ToolResult<String> {
    string_field(input, key).ok_or_else(|| {
        invalid(format!(
            "Parameter `{key}` is required for command: {command}"
        ))
    })
}

fn command_of(input: &Value) -> ToolResult<String> {
    let command = string_field(input, "command")
        .ok_or_else(|| invalid("Parameter `command` is required for str_replace_editor"))?;
    match command.as_str() {
        "view" | "create" | "str_replace" | "insert" => Ok(command),
        other => Err(invalid(format!(
            "Unrecognized command {other}. The allowed commands for the str_replace_editor tool are: view, create, str_replace, insert"
        ))),
    }
}

fn path_of(input: &Value) -> ToolResult<PathBuf> {
    let raw = required_field(input, "path", "str_replace_editor")?;
    if raw.trim().is_empty() {
        return Err(invalid("path must be a non-empty string"));
    }
    let path = PathBuf::from(&raw);
    if !path.is_absolute() {
        return Err(invalid(format!(
            "The path {raw} is not an absolute path, it should start with a filesystem root."
        )));
    }
    Ok(path)
}

fn view_range_of(input: &Value) -> ToolResult<Option<(i64, i64)>> {
    let Some(value) = input.get("view_range") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let entries = value
        .as_array()
        .ok_or_else(|| invalid("Invalid `view_range`. It should be a list of two integers."))?;
    let numbers = entries
        .iter()
        .map(Value::as_i64)
        .collect::<Option<Vec<_>>>()
        .filter(|numbers| numbers.len() == 2)
        .ok_or_else(|| invalid("Invalid `view_range`. It should be a list of two integers."))?;
    let (start, end) = (numbers[0], numbers[1]);
    if start < 1 {
        return Err(invalid(format!(
            "Invalid `view_range`: [{start}, {end}]. Its first element `{start}` should be within the range of lines of the file"
        )));
    }
    if end != -1 && end < start {
        return Err(invalid(format!(
            "Invalid `view_range`: [{start}, {end}]. Its second element `{end}` should be larger or equal than its first `{start}`"
        )));
    }
    Ok(Some((start, end)))
}

/// Translate one `str_replace_editor` call into a native tool call.
///
/// `for_validation` keeps the expensive part of `insert` (reading the file to
/// splice it) out of the validation pass, which runs before permissions.
fn translate(input: &Value, for_validation: bool) -> ToolResult<Translated> {
    let command = command_of(input)?;
    let path = path_of(input)?;
    let display = path.display().to_string();
    let metadata = std::fs::metadata(&path).ok();

    if command != "create" && metadata.is_none() {
        return Err(invalid(format!(
            "The path {display} does not exist. Please provide a valid path."
        )));
    }
    if command != "view" && metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) {
        return Err(invalid(format!(
            "The path {display} is a directory and only the `view` command can be used on directories"
        )));
    }

    match command.as_str() {
        "view" => {
            if metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) {
                if input
                    .get("view_range")
                    .is_some_and(|value| !value.is_null())
                {
                    return Err(invalid(
                        "The `view_range` parameter is not allowed when `path` points to a directory.",
                    ));
                }
                return Ok(Translated {
                    target: Target::ListDirectory(path),
                    input: Value::Null,
                });
            }
            let mut read = json!({ "file_path": display });
            if let Some((start, end)) = view_range_of(input)? {
                read["offset"] = json!(start);
                if end != -1 {
                    read["limit"] = json!(end - start + 1);
                }
            }
            Ok(Translated {
                target: Target::Read,
                input: read,
            })
        }
        "create" => {
            let file_text = required_field(input, "file_text", "create")?;
            if metadata.is_some() {
                return Err(invalid(format!(
                    "File already exists at: {display}. Cannot overwrite files using command `create`."
                )));
            }
            Ok(Translated {
                target: Target::Write,
                input: json!({ "file_path": display, "content": file_text }),
            })
        }
        "str_replace" => {
            let old_str = required_field(input, "old_str", "str_replace")?;
            let new_str = string_field(input, "new_str").unwrap_or_default();
            Ok(Translated {
                target: Target::Edit,
                input: json!({
                    "file_path": display,
                    "old_string": old_str,
                    "new_string": new_str,
                }),
            })
        }
        _ => {
            let new_str = required_field(input, "new_str", "insert")?;
            let insert_line = input
                .get("insert_line")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    invalid("Parameter `insert_line` is required for command: insert")
                })?;
            let content = if for_validation {
                String::new()
            } else {
                splice_insert(&path, insert_line, &new_str)?
            };
            Ok(Translated {
                target: Target::Write,
                input: json!({ "file_path": display, "content": content }),
            })
        }
    }
}

/// Build the post-insert file body. `insert_line` is 1-based and names the line
/// the new text is placed AFTER; `0` prepends.
fn splice_insert(path: &Path, insert_line: i64, new_str: &str) -> ToolResult<String> {
    let original = std::fs::read_to_string(path).map_err(|err| {
        invalid(format!(
            "cannot read {} for command `insert`: {err}",
            path.display()
        ))
    })?;
    let trailing_newline = original.ends_with('\n');
    let body = if trailing_newline {
        original.strip_suffix('\n').unwrap_or_default()
    } else {
        original.as_str()
    };
    let mut lines: Vec<&str> = if body.is_empty() {
        Vec::new()
    } else {
        body.split('\n').collect()
    };
    if insert_line < 0 || insert_line as usize > lines.len() {
        return Err(invalid(format!(
            "Invalid `insert_line` parameter: {insert_line}. It should be within the range of lines of the file: [0, {}]",
            lines.len()
        )));
    }
    let inserted: Vec<&str> = new_str
        .strip_suffix('\n')
        .unwrap_or(new_str)
        .split('\n')
        .collect();
    let at = insert_line as usize;
    for (offset, line) in inserted.into_iter().enumerate() {
        lines.insert(at + offset, line);
    }
    let mut result = lines.join("\n");
    if trailing_newline || !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

/// Rewrite a delegate's recovery advice into this tool's vocabulary.
///
/// Rebon requires a `Read` before an edit; upstream's editor does not. A model
/// on the bootstrap request has never been shown `Read` or `Edit`, so telling
/// it to "use the Read tool, then retry the Edit" names two tools it cannot
/// call. The precondition is real and stays enforced — only the instruction is
/// translated.
fn retarget_guidance(outcome: ValidationOutcome, translated: &Translated) -> ValidationOutcome {
    if outcome.is_valid() {
        return outcome;
    }
    let Some(message) = outcome.message else {
        return ValidationOutcome {
            result: outcome.result,
            message: None,
            error_code: outcome.error_code,
        };
    };
    let path = translated
        .input
        .get("file_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let native = match translated.target {
        Target::Edit => "Edit",
        Target::Write => "Write",
        _ => {
            return ValidationOutcome {
                result: outcome.result,
                message: Some(message),
                error_code: outcome.error_code,
            }
        }
    };
    let rewritten = message
        .replace(
            &format!("Use the Read tool with file_path={path} first"),
            &format!(
                "Run `{STR_REPLACE_EDITOR_TOOL_NAME}` with command=view and path={path} first"
            ),
        )
        .replace(&format!("then retry the {native}"), "then retry this edit")
        .replace(
            &format!("Read the file again and retry the {native}"),
            "View the file again and retry this edit",
        );
    ValidationOutcome {
        result: outcome.result,
        message: Some(rewritten),
        error_code: outcome.error_code,
    }
}

/// Serve `view` on a directory: two levels deep, hidden entries and the two
/// cache directories the upstream tool also skips are omitted.
fn list_directory(root: &Path) -> ToolResult<Value> {
    fn visit(dir: &Path, depth: usize, rows: &mut Vec<String>) -> ToolResult<()> {
        let entries = std::fs::read_dir(dir)
            .map_err(|err| invalid(format!("cannot list {}: {err}", dir.display())))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" || name == "__pycache__" {
                continue;
            }
            let path = entry.path();
            let is_dir = path.is_dir();
            rows.push(format!(
                "{}\t{}",
                if is_dir { 'd' } else { 'f' },
                path.display()
            ));
            if is_dir && depth < LIST_DEPTH {
                visit(&path, depth + 1, rows)?;
            }
        }
        Ok(())
    }

    let mut rows = vec![format!("d\t{}", root.display())];
    visit(root, 1, &mut rows)?;
    rows.sort_by(|left, right| {
        let left_path = left.split_once('\t').map(|(_, path)| path).unwrap_or(left);
        let right_path = right
            .split_once('\t')
            .map(|(_, path)| path)
            .unwrap_or(right);
        left_path.cmp(right_path)
    });
    Ok(json!({
        "content": format!(
            "Here're the files and directories up to {LIST_DEPTH} levels deep in {}, excluding hidden items, node_modules, and Python cache directories:\n{}\n",
            root.display(),
            rows.join("\n"),
        )
    }))
}

#[async_trait]
impl Tool for StrReplaceEditorTool {
    fn id(&self) -> ToolId {
        ToolId::new(STR_REPLACE_EDITOR_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["StrReplaceEditor"]
    }

    fn description(&self) -> &str {
        STR_REPLACE_EDITOR_DESCRIPTION
    }

    fn input_schema(&self) -> ToolInputSchema {
        str_replace_editor_input_schema()
    }

    /// Only the Anchored Minimal bootstrap request projects this tool; every
    /// other surface keeps Rebon's native `Read`/`Write`/`Edit`.
    fn should_defer(&self) -> bool {
        true
    }

    fn is_read_only(&self, input: &Value) -> bool {
        string_field(input, "command").as_deref() == Some("view")
    }

    fn is_destructive(&self, input: &Value) -> bool {
        !self.is_read_only(input)
    }

    fn needs_permission(&self, input: &Value) -> bool {
        !self.is_read_only(input)
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let translated = match translate(input, true) {
            Ok(translated) => translated,
            refused => return validation_outcome_from(refused),
        };
        match translated.tool() {
            Some(tool) => Ok(retarget_guidance(
                tool.validate_input(&translated.input, context).await?,
                &translated,
            )),
            None => Ok(ValidationOutcome::valid()),
        }
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let translated = translate(input, true)?;
        let Some(tool) = translated.tool() else {
            return Ok(PermissionDecision::allow(input.clone()));
        };
        // Reuse the delegate's prompt copy and auto-allow rules, but keep the
        // model-authored input on the decision: the broker replays it into
        // `call`, which does its own translation.
        let decision = tool.check_permissions(&translated.input, context).await?;
        Ok(match decision.behavior {
            PermissionBehavior::Allow => PermissionDecision::allow(input.clone()),
            PermissionBehavior::Deny => PermissionDecision::deny(
                decision
                    .reason
                    .unwrap_or_else(|| "tool permission denied".into()),
            ),
            PermissionBehavior::Ask => match decision.request {
                Some(request) => PermissionDecision::ask(request, Some(input.clone())),
                None => PermissionDecision::allow(input.clone()),
            },
        })
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let translated = translate(&input, false)?;
        match translated.target {
            Target::ListDirectory(ref root) => list_directory(root),
            _ => {
                let tool = translated
                    .tool()
                    .expect("non-listing targets resolve to a native tool");
                tool.call(translated.input, context).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn schema_matches_the_upstream_minimal_identity() {
        let schema = str_replace_editor_input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["command", "path"]));
        assert!(
            schema.get("additionalProperties").is_none(),
            "upstream emits no additionalProperties; adding one changes the anchor"
        );
        let properties = schema["properties"].as_object().expect("properties");
        // Declaration order is written to match upstream, but `serde_json`
        // sorts object keys unless `preserve_order` is on (the desktop app
        // enables it, the CLI does not), so only the key set is assertable.
        let mut keys = properties.keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "command",
                "file_text",
                "insert_line",
                "new_str",
                "old_str",
                "path",
                "view_range"
            ]
        );
        assert_eq!(
            properties["command"]["enum"],
            json!(["view", "create", "str_replace", "insert"])
        );
    }

    #[test]
    fn view_translates_range_to_read_offset_and_limit() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "1\n2\n3\n4\n").unwrap();
        let translated = translate(
            &json!({ "command": "view", "path": file, "view_range": [2, 3] }),
            false,
        )
        .unwrap();
        assert!(matches!(translated.target, Target::Read));
        assert_eq!(translated.input["offset"], 2);
        assert_eq!(translated.input["limit"], 2);
    }

    #[test]
    fn open_ended_view_range_omits_limit() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "1\n2\n3\n").unwrap();
        let translated = translate(
            &json!({ "command": "view", "path": file, "view_range": [2, -1] }),
            false,
        )
        .unwrap();
        assert_eq!(translated.input["offset"], 2);
        assert!(translated.input.get("limit").is_none());
    }

    #[test]
    fn str_replace_translates_to_edit_and_defaults_new_str() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let translated = translate(
            &json!({ "command": "str_replace", "path": file, "old_str": "hello" }),
            false,
        )
        .unwrap();
        assert!(matches!(translated.target, Target::Edit));
        assert_eq!(translated.input["old_string"], "hello");
        assert_eq!(translated.input["new_string"], "");
    }

    #[test]
    fn create_refuses_to_overwrite_an_existing_file() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let err = translate(
            &json!({ "command": "create", "path": file, "file_text": "x" }),
            false,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("Cannot overwrite files using command `create`"));
    }

    #[test]
    fn insert_splices_after_the_named_line() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let translated = translate(
            &json!({ "command": "insert", "path": file, "insert_line": 1, "new_str": "mid" }),
            false,
        )
        .unwrap();
        assert!(matches!(translated.target, Target::Write));
        assert_eq!(translated.input["content"], "one\nmid\ntwo\n");
    }

    #[test]
    fn insert_line_zero_prepends() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let translated = translate(
            &json!({ "command": "insert", "path": file, "insert_line": 0, "new_str": "top" }),
            false,
        )
        .unwrap();
        assert_eq!(translated.input["content"], "top\none\n");
    }

    #[test]
    fn insert_past_the_end_is_rejected() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let err = translate(
            &json!({ "command": "insert", "path": file, "insert_line": 9, "new_str": "x" }),
            false,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("Invalid `insert_line` parameter"));
    }

    #[test]
    fn directory_view_lists_two_levels_and_skips_hidden_entries() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("a/b/c/deep.txt"), "").unwrap();
        std::fs::write(dir.path().join("a/top.txt"), "").unwrap();
        let listing = list_directory(dir.path()).unwrap();
        let text = listing["content"].as_str().unwrap();
        assert!(text.contains("top.txt"));
        assert!(!text.contains(".git"));
        assert!(
            !text.contains("deep.txt"),
            "third level must not be listed: {text}"
        );
    }

    #[test]
    fn missing_path_reports_the_upstream_message() {
        let dir = tempdir();
        let err = translate(
            &json!({ "command": "view", "path": dir.path().join("nope.txt") }),
            false,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("does not exist. Please provide a valid path."));
    }

    #[test]
    fn relative_paths_are_rejected() {
        let err = translate(&json!({ "command": "view", "path": "notes.txt" }), false).unwrap_err();
        assert!(format!("{err}").contains("is not an absolute path"));
    }

    #[tokio::test]
    async fn create_writes_the_file_through_the_write_tool() {
        let dir = tempdir();
        let file = dir.path().join("new.txt");
        let tool = StrReplaceEditorTool;
        let input = json!({ "command": "create", "path": file, "file_text": "hello\n" });
        assert!(tool
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap()
            .is_valid());
        tool.call(input, &ToolContext::new()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn view_delegates_to_read_and_returns_its_payload() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();
        let result = StrReplaceEditorTool
            .call(
                json!({ "command": "view", "path": file }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&result).unwrap().contains("alpha"),
            "view should surface file content: {result}"
        );
    }

    #[tokio::test]
    async fn view_on_a_directory_is_served_without_a_delegate() {
        let dir = tempdir();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let result = StrReplaceEditorTool
            .call(
                json!({ "command": "view", "path": dir.path() }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(result["content"].as_str().unwrap().contains("a.txt"));
    }

    #[tokio::test]
    async fn read_before_edit_advice_is_translated_into_this_tool_vocabulary() {
        let dir = tempdir();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha\n").unwrap();
        let outcome = StrReplaceEditorTool
            .validate_input(
                &json!({
                    "command": "str_replace",
                    "path": file,
                    "old_str": "alpha",
                    "new_str": "beta",
                }),
                &ToolContext::new().with_file_state_cache(Default::default()),
            )
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        let message = outcome.message.unwrap();
        assert!(
            message.contains("str_replace_editor` with command=view"),
            "recovery advice must name a tool the bootstrap catalog exposes: {message}"
        );
        assert!(
            !message.contains("Use the Read tool"),
            "stale advice leaked: {message}"
        );
    }

    #[test]
    fn mutating_commands_need_permission_and_view_does_not() {
        let tool = StrReplaceEditorTool;
        assert!(!tool.needs_permission(&json!({ "command": "view", "path": "/tmp/a" })));
        assert!(tool.needs_permission(&json!({ "command": "str_replace", "path": "/tmp/a" })));
        assert!(tool.is_read_only(&json!({ "command": "view", "path": "/tmp/a" })));
    }
}
