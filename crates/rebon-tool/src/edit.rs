use crate::{Tool, ToolContext};
use async_trait::async_trait;
use rebon_tools_core::{
    require_valid_input, validation_outcome_from, PermissionDecision, PermissionRequest, ToolError,
    ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE_EDIT_TOOL_NAME: &str = "Edit";
pub const INVALID_INPUT_CODE: i64 = 400;
pub const NOT_FOUND_CODE: i64 = 1;
pub const NOT_FILE_CODE: i64 = 2;
pub(crate) const IDENTICAL_STRINGS_CODE: i64 = 3;
pub(crate) const OLD_STRING_NOT_FOUND_CODE: i64 = 4;
pub(crate) const AMBIGUOUS_MATCH_CODE: i64 = 5;
pub(crate) const UNSUPPORTED_MEDIA_CODE: i64 = 8;

#[derive(Debug, Clone, Default)]
pub struct EditTool;

#[derive(Debug, Clone)]
struct EditInput {
    file_path: PathBuf,
    old_string: String,
    new_string: String,
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn id(&self) -> ToolId {
        ToolId::new(FILE_EDIT_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["FileEditTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::FileEdit
    }

    fn file_target_field(&self) -> Option<&'static str> {
        Some("file_path")
    }

    fn description(&self) -> &str {
        "Performs exact string replacements in files.\n\
         \n\
         Usage:\n\
         - When editing text from Read tool output, ensure you preserve the exact indentation \
         (tabs/spaces) as it appears AFTER the line number prefix. The line number prefix format \
         is: line number + tab. Everything after that is the actual file content to match. \
         Never include any part of the line number prefix in the old_string or new_string.\n\
         - ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.\n\
         - Only use emojis if the user explicitly requests it. Avoid adding emojis to files unless asked.\n\
         - The edit will FAIL if `old_string` is not unique in the file. Either provide a larger string \
         with more surrounding context to make it unique or use `replace_all` to change every instance of `old_string`.\n\
         - Use `replace_all` for replacing and renaming strings across the file. This parameter is useful \
         if you want to rename a variable for instance."
    }

    fn model_description(&self) -> &str {
        if crate::legacy_prompt_profile() {
            self.description()
        } else {
            LEAN_DESCRIPTION
        }
    }

    /// A relative `file_path` is taken from the working directory, before the
    /// rules judge it.
    fn normalize_input(&self, mut input: Value, context: &ToolContext) -> Value {
        if let Some(object) = input.as_object_mut() {
            crate::path_scope::absolutize_input_field(object, "file_path", context);
        }
        input
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean" }
            },
            "required": ["file_path", "old_string", "new_string"],
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
        validation_outcome_from(prepare_input(self.id(), input, context))
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

        Ok(PermissionDecision::ask(
            PermissionRequest::new("Edit file", format!("Edit wants to modify: {path}"))
                .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = prepare_input(self.id(), &input, context)?;

        // `old_string` is matched against the file as it is now, under the
        // path lock — not against what an earlier Read saw — so a formatter
        // or another writer touching the file is not by itself a failure.
        let _file_guard = crate::lock_file_for_write(&parsed.file_path).await;

        let old_content = match fs::read(&parsed.file_path) {
            Ok(bytes) => Some(
                String::from_utf8(bytes).map_err(|_| ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!(
                        "Unsupported non-text file in current Rust Edit slice: {}",
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

        let (updated, edit_type, replacements, original_text) = match old_content {
            Some(content) if parsed.old_string.is_empty() => {
                if !content.is_empty() {
                    return Err(ToolError::InvalidInput {
                        tool: self.id(),
                        reason: format!(
                            "{} already exists and is not empty. An empty `old_string` only \
                             creates a file; put the text to replace in `old_string`.",
                            parsed.file_path.display()
                        ),
                        error_code: Some(OLD_STRING_NOT_FOUND_CODE),
                    });
                }
                (parsed.new_string.clone(), "update", 1, Some(content))
            }
            Some(content) => {
                // Line endings follow the file: models emit bare LF, files
                // on Windows are often CRLF.
                let anchor = crate::edit_match::find_anchor(
                    &content,
                    &parsed.old_string,
                    &parsed.new_string,
                );
                let occurrences = anchor.occurrences;
                if occurrences == 0 {
                    return Err(ToolError::InvalidInput {
                        tool: self.id(),
                        reason: format!(
                            "`old_string` was not found in {}. {}",
                            parsed.file_path.display(),
                            crate::edit_match::closest_region_report(&content, &parsed.old_string)
                        ),
                        error_code: Some(OLD_STRING_NOT_FOUND_CODE),
                    });
                }
                if occurrences > 1 && !parsed.replace_all {
                    return Err(ToolError::InvalidInput {
                        tool: self.id(),
                        reason: ambiguous_match_reason(
                            &format!("in {}", parsed.file_path.display()),
                            &content,
                            &anchor.old,
                            occurrences,
                        ),
                        error_code: Some(AMBIGUOUS_MATCH_CODE),
                    });
                }
                let updated = if parsed.replace_all {
                    content.replace(&*anchor.old, &anchor.new)
                } else {
                    content.replacen(&*anchor.old, &anchor.new, 1)
                };
                (
                    updated,
                    "update",
                    if parsed.replace_all { occurrences } else { 1 },
                    Some(content),
                )
            }
            None => {
                if !parsed.old_string.is_empty() {
                    return Err(ToolError::InvalidInput {
                        tool: self.id(),
                        reason: format!(
                            "File does not exist: {}. Use empty `old_string` only when creating a new file via Edit.",
                            parsed.file_path.display()
                        ),
                        error_code: Some(NOT_FOUND_CODE),
                    });
                }
                (parsed.new_string.clone(), "create", 1, None)
            }
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

        // Record what this session just wrote. Creating the file, or filling
        // an empty one, writes every byte, so the file now counts as seen;
        // replacing a span shows the model only that span, so it refreshes an
        // entry that is there and makes no unseen file seen.
        if let Some(cache) = context.file_state_cache() {
            if parsed.old_string.is_empty() {
                cache.record_whole_write(&parsed.file_path, updated.clone());
            } else {
                cache.record_partial_write(&parsed.file_path, updated.clone());
            }
        }
        crate::record_file_change(context, &parsed.file_path);

        let result = json!({
            "type": edit_type,
            "filePath": normalize_path(&parsed.file_path),
            "oldString": parsed.old_string,
            "newString": parsed.new_string,
            "replacements": replacements,
            "content": updated,
            "originalFile": original_text,
        });

        Ok(result)
    }
}

/// Parse and vet one `Edit` request, once.
///
/// Both entry points ask the same three questions in the same order — does
/// the input parse, does the path clear the edit scope, is the parsed request
/// well-formed. Whether `old_string` matches is answered only by `call`,
/// against the content it is about to rewrite.
fn prepare_input(tool: ToolId, input: &Value, context: &ToolContext) -> ToolResult<EditInput> {
    let mut parsed = parse_input(input)?;
    parsed.file_path = crate::path_scope::absolute_from_cwd(parsed.file_path, context);
    enforce_edit_path_scope(tool.clone(), &parsed.file_path, context)?;
    require_valid_input(
        tool,
        validate_parsed_input(&parsed)?,
        "Edit input is invalid",
    )?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<EditInput> {
    let tool = ToolId::new(FILE_EDIT_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Edit input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    Ok(EditInput {
        file_path: PathBuf::from(required_string(
            object.get("file_path"),
            "file_path",
            &tool,
        )?),
        old_string: required_string(object.get("old_string"), "old_string", &tool)?,
        new_string: required_string(object.get("new_string"), "new_string", &tool)?,
        replace_all: optional_bool(object.get("replace_all"), "replace_all", &tool)?
            .unwrap_or(false),
    })
}

fn validate_parsed_input(input: &EditInput) -> ToolResult<ValidationOutcome> {
    if !input.file_path.is_absolute() {
        return Ok(ValidationOutcome::invalid(
            format!(
                "Edit requires an absolute `file_path`, got: {}. \
                 Examples: C:\\Users\\name\\file.rs or D:/project/file.rs \
                 on Windows; /home/name/file.rs on Linux/macOS.",
                input.file_path.display()
            ),
            INVALID_INPUT_CODE,
        ));
    }

    if input.old_string == input.new_string {
        return Ok(ValidationOutcome::invalid(
            "No changes to make: `old_string` and `new_string` are identical",
            IDENTICAL_STRINGS_CODE,
        ));
    }

    match fs::metadata(&input.file_path) {
        Ok(metadata) if !metadata.is_file() => Ok(ValidationOutcome::invalid(
            format!("Path is not a file: {}", input.file_path.display()),
            NOT_FILE_CODE,
        )),
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if input.old_string.is_empty() {
                Ok(ValidationOutcome::valid())
            } else {
                Ok(ValidationOutcome::invalid(
                    format!("File does not exist: {}", input.file_path.display()),
                    NOT_FOUND_CODE,
                ))
            }
        }
        Err(err) => Err(ToolError::Execution {
            tool: ToolId::new(FILE_EDIT_TOOL_NAME),
            source: err.into(),
        }),
    }
}

fn required_string(value: Option<&Value>, field: &str, tool: &ToolId) -> ToolResult<String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("Edit input requires a string `{field}`"),
            error_code: Some(INVALID_INPUT_CODE),
        })
        .map(ToOwned::to_owned)
}

/// Read an optional boolean field from a tool's input, rejecting any
/// other JSON type. Shared by the tools that take flag-style options.
pub(crate) fn optional_bool(
    value: Option<&Value>,
    field: &str,
    tool: &ToolId,
) -> ToolResult<Option<bool>> {
    match value {
        Some(Value::Bool(raw)) => Ok(Some(*raw)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("`{field}` must be a boolean when provided"),
            error_code: Some(INVALID_INPUT_CODE),
        }),
    }
}

pub fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn is_auto_memory_path(path: &Path, context: &ToolContext) -> bool {
    context.cwd().is_some_and(|cwd| {
        rebon_session::memory_paths::is_memory_path_for_any_scope(&path.to_string_lossy(), cwd)
    })
}

/// Whether `path` lies in a root this session may already write to without
/// asking: an explicit write scope, or the auto-approved roots (the
/// scratchpad). Never a git metadata path, even inside such a root — a
/// scratchpad someone ran `git init` in is a repository whose config the
/// next unasked `git status` would execute (see
/// [`crate::path_scope::is_git_metadata_path`]).
pub fn explicitly_authorized_write_path(path: &Path, context: &ToolContext) -> bool {
    let path = context
        .cwd()
        .map(|cwd| crate::path_scope::resolve_context_path(path, Path::new(cwd), context))
        .unwrap_or_else(|| path.to_path_buf());
    if crate::path_scope::is_git_metadata_path(&path) {
        return false;
    }
    context
        .write_scope_roots()
        .is_some_and(|roots| crate::path_scope::mutation_path_is_within_roots(&path, roots))
        || crate::path_scope::mutation_path_is_within_roots(
            &path,
            context.auto_approved_write_roots(),
        )
}

/// Sub-agent mutation-scope gate for edits. An explicit write scope
/// is authoritative, including over the ordinary auto-memory carve-out.
///
/// Shared by every tool in the file-edit permission class (`Edit`,
/// `MultiEdit`, `NotebookEdit`) so a scoped worker cannot reach outside
/// its roots through whichever edit tool it happens to pick.
pub fn enforce_edit_path_scope(tool: ToolId, path: &Path, context: &ToolContext) -> ToolResult<()> {
    if context.write_scope_roots().is_none() && is_auto_memory_path(path, context) {
        return Ok(());
    }
    crate::path_scope::enforce_write_path_policy(tool, context, path, "file_path")
}

/// The error for an `old_string` that matches more than once: where each
/// match is, and how to pick one. `whose_text` names the text the line
/// numbers belong to (`in /path`); `MultiEdit` prefixes the failing edit's
/// index.
pub(crate) fn ambiguous_match_reason(
    whose_text: &str,
    content: &str,
    needle: &str,
    occurrences: usize,
) -> String {
    format!(
        "`old_string` matched {occurrences} times {whose_text}; it must match exactly once.\n{}\
         Add surrounding lines to `old_string` to pick one, or set `replace_all` to true to \
         replace every occurrence.",
        crate::edit_match::match_listing(content, needle, occurrences)
    )
}

/// Adapt a model-supplied string to match the file's line-ending style.
/// Models almost always emit bare `\n`; on Windows files often use `\r\n`.
pub(crate) fn normalize_line_endings(s: &str, to_crlf: bool) -> std::borrow::Cow<'_, str> {
    if to_crlf {
        // First strip any existing \r\n to avoid doubling, then convert \n → \r\n.
        let clean = s.replace("\r\n", "\n");
        std::borrow::Cow::Owned(clean.replace('\n', "\r\n"))
    } else {
        // File uses LF — strip any stray \r the model might have sent.
        if s.contains("\r\n") {
            std::borrow::Cow::Owned(s.replace("\r\n", "\n"))
        } else {
            std::borrow::Cow::Borrowed(s)
        }
    }
}

/// Provider-visible description under the lean prompt profile.
const LEAN_DESCRIPTION: &str =
    "Replace `old_string` with `new_string` in a file (absolute `file_path`). \
`old_string` is matched against the file's current content: exactly, without Read's \
line-number prefix, and uniquely unless `replace_all` is true. A miss returns the nearby \
current lines. Several edits to one file in one response apply in order.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::test_support::TestConfigHome;
    use rebon_tools_core::file_state::FileState;
    use rebon_tools_core::file_state::FileStateCache;
    use rebon_tools_core::PermissionBehavior;
    use serde_json::json;
    use std::sync::Arc;

    struct TempDir {
        inner: tempfile::TempDir,
    }

    impl TempDir {
        fn new() -> Self {
            Self {
                inner: tempfile::Builder::new()
                    .prefix("rebon-edit-tool-test-")
                    .tempdir()
                    .unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }
    }

    fn tool() -> EditTool {
        EditTool
    }

    #[tokio::test]
    async fn validate_input_rejects_identical_strings() {
        let dir = TempDir::new();
        let file = dir.path().join("a.txt");
        fs::write(&file, "same").unwrap();

        let result = tool()
            .validate_input(
                &json!({
                    "file_path": file.to_string_lossy(),
                    "old_string": "same",
                    "new_string": "same"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.error_code, Some(IDENTICAL_STRINGS_CODE));
    }

    #[tokio::test]
    async fn call_replaces_single_occurrence() {
        let dir = TempDir::new();
        let file = dir.path().join("demo.txt");
        fs::write(&file, "alpha beta gamma").unwrap();

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "old_string": "beta",
                    "new_string": "BETA"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("update"));
        assert_eq!(out["replacements"], json!(1));
        assert_eq!(fs::read_to_string(&file).unwrap(), "alpha BETA gamma");
    }

    #[tokio::test]
    async fn call_requires_replace_all_for_ambiguous_match() {
        let dir = TempDir::new();
        let file = dir.path().join("demo.txt");
        fs::write(&file, "x x").unwrap();

        let err = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "old_string": "x",
                    "new_string": "y"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { error_code, .. } => {
                assert_eq!(error_code, Some(AMBIGUOUS_MATCH_CODE))
            }
            other => panic!("expected invalid input, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_creates_file_when_old_string_is_empty() {
        let dir = TempDir::new();
        let file = dir.path().join("new.txt");

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "old_string": "",
                    "new_string": "fresh"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("create"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "fresh");
    }

    #[tokio::test]
    async fn scratchpad_auto_approval_does_not_auto_approve_project_edits() {
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("general-purpose")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project.clone(), scratchpad.clone()])
            .with_auto_approved_write_roots([scratchpad.clone()]);

        let scratchpad_input = json!({
            "file_path": scratchpad.join("result.txt"),
            "old_string": "",
            "new_string": "artifact",
        });
        let project_input = json!({
            "file_path": project.join("src.txt"),
            "old_string": "before",
            "new_string": "after",
        });

        assert_eq!(
            tool()
                .check_permissions(&scratchpad_input, &ctx)
                .await
                .unwrap()
                .behavior,
            PermissionBehavior::Allow
        );
        assert_eq!(
            tool()
                .check_permissions(&project_input, &ctx)
                .await
                .unwrap()
                .behavior,
            PermissionBehavior::Ask
        );
    }

    #[tokio::test]
    async fn explicit_write_scope_allows_scratchpad_edit_without_permission_prompt() {
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
            "old_string": "before",
            "new_string": "after",
        });

        let decision = tool().check_permissions(&input, &ctx).await.unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Allow);
        tool().call(input, &ctx).await.unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "after");
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
                    "old_string": "before",
                    "new_string": "after",
                }),
                &ctx,
            )
            .await;

        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
        assert_eq!(fs::read_to_string(file).unwrap(), "before");
    }

    #[tokio::test]
    async fn explicit_write_scope_rejects_auto_memory_carveout() {
        let _config_home = TestConfigHome::new("edit-explicit-scope-memory");
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let memory_file = rebon_session::memory_paths::repo_memory_dir(&project.to_string_lossy())
            .unwrap()
            .join("MEMORY.md");
        let ctx = ToolContext::new()
            .with_agent_id("verification")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);

        let result = tool()
            .call(
                json!({
                    "file_path": memory_file.to_string_lossy(),
                    "old_string": "",
                    "new_string": "nope",
                }),
                &ctx,
            )
            .await;

        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
        assert!(!memory_file.exists());
    }

    #[tokio::test]
    async fn edit_to_auto_memory_path_is_permission_allowed() {
        let config_home = TestConfigHome::new("edit-memory-permission");
        let dir = TempDir::new();
        let cwd = dir.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let memory_file = rebon_session::memory_paths::repo_memory_dir(&cwd.to_string_lossy())
            .unwrap()
            .join("MEMORY.md");
        let input = json!({
            "file_path": memory_file.to_string_lossy(),
            "old_string": "before",
            "new_string": "after",
        });
        let decision = tool()
            .check_permissions(&input, &ToolContext::new().with_cwd(cwd.to_string_lossy()))
            .await
            .unwrap();

        assert_eq!(decision.behavior, PermissionBehavior::Allow);
        assert_eq!(decision.updated_input, Some(input));
        assert!(memory_file.starts_with(config_home.path()));
    }

    #[derive(Default)]
    struct RecordingHistoryTracker {
        calls: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl rebon_agent_core::file_history::FileHistoryTracker for RecordingHistoryTracker {
        fn track_before_write(&self, file_path: &Path) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(file_path.to_path_buf());
            Ok(())
        }
    }

    #[tokio::test]
    async fn edit_to_auto_memory_path_skips_file_history_tracking() {
        let _config_home = TestConfigHome::new("edit-memory-history-skip");
        let dir = TempDir::new();
        let cwd = dir.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let memory_file = rebon_session::memory_paths::repo_memory_dir(&cwd.to_string_lossy())
            .unwrap()
            .join("MEMORY.md");
        fs::create_dir_all(memory_file.parent().unwrap()).unwrap();
        fs::write(&memory_file, "before").unwrap();
        let tracker = Arc::new(RecordingHistoryTracker::default());
        let ctx = ToolContext::new()
            .with_cwd(cwd.to_string_lossy())
            .with_file_history_tracker(tracker.clone());

        tool()
            .call(
                json!({
                    "file_path": memory_file.to_string_lossy(),
                    "old_string": "before",
                    "new_string": "after"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(tracker
            .calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
    }

    /// The shell-first toolset creates files with Edit alone: an empty
    /// `old_string` makes the file and any missing directories, file history
    /// sees it before it exists (so rewind can remove it), and the same call
    /// against a file that has content is refused rather than overwriting it.
    #[tokio::test]
    async fn an_empty_old_string_creates_a_file_in_new_directories_under_history() {
        let dir = TempDir::new();
        let file = dir.path().join("fresh").join("dir").join("new.rs");
        let tracker = Arc::new(RecordingHistoryTracker::default());
        let ctx = ToolContext::new()
            .with_cwd(dir.path().to_string_lossy())
            .with_file_history_tracker(tracker.clone());

        let out = tool()
            .call(edit_input(&file, "", "fn main() {}\n"), &ctx)
            .await
            .unwrap();
        assert_eq!(out["type"], json!("create"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "fn main() {}\n");
        assert_eq!(
            *tracker
                .calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![file.clone()]
        );

        let err = tool()
            .call(edit_input(&file, "", "other"), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists and is not empty"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "fn main() {}\n");
    }

    #[tokio::test]
    async fn call_matches_lf_old_string_against_crlf_file() {
        let dir = TempDir::new();
        let file = dir.path().join("crlf.txt");
        // File on disk uses CRLF line endings.
        fs::write(&file, "line1\r\nline2\r\nline3\r\n").unwrap();

        // Model sends old_string with bare LF — should still match.
        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "old_string": "line1\nline2",
                    "new_string": "LINE1\nLINE2"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("update"));
        // The replacement should preserve CRLF style.
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "LINE1\r\nLINE2\r\nline3\r\n"
        );
    }

    #[tokio::test]
    async fn call_replace_all_with_crlf_normalization() {
        let dir = TempDir::new();
        let file = dir.path().join("crlf2.txt");
        fs::write(&file, "aa\r\nbb\r\naa\r\nbb\r\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "old_string": "aa\nbb",
                    "new_string": "XX\nYY",
                    "replace_all": true
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("update"));
        assert_eq!(out["replacements"], json!(2));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "XX\r\nYY\r\nXX\r\nYY\r\n"
        );
    }

    /// Helper: pre-populate the cache as if Read had just observed
    /// the file.
    fn register_read(cache: &FileStateCache, path: &Path, content: &str) {
        cache.set(
            path,
            FileState {
                content: content.to_string(),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
    }

    fn edit_input(file: &Path, old: &str, new: &str) -> Value {
        json!({
            "file_path": file.to_string_lossy(),
            "old_string": old,
            "new_string": new
        })
    }

    fn invalid_reason(err: ToolError, expected_code: i64) -> String {
        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(expected_code), "{reason}");
                reason
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// Edit has no read-state precondition: a session cache with no entry
    /// for the file does not stop it. The model saw only the span it
    /// replaced, so the edit does not make the file count as seen, and a
    /// later Write still needs a Read or `overwrite`.
    #[tokio::test]
    async fn edit_applies_to_a_file_this_session_never_read() {
        let dir = TempDir::new();
        let file = dir.path().join("unseen.txt");
        fs::write(&file, "hello world").unwrap();
        let cache = FileStateCache::new();
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(edit_input(&file, "hello", "HELLO"), &ctx)
            .await
            .expect("no Read is required before an Edit");

        assert_eq!(fs::read_to_string(&file).unwrap(), "HELLO world");
        assert!(cache.get(&file).is_none(), "a blind edit is not a read");
        let err = crate::WriteTool
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "replaced" }),
                &ctx,
            )
            .await
            .unwrap_err();
        invalid_reason(err, crate::write::UNSEEN_OVERWRITE_CODE);
        assert_eq!(fs::read_to_string(&file).unwrap(), "HELLO world");
    }

    #[tokio::test]
    async fn a_relative_path_is_taken_from_the_working_directory() {
        let dir = TempDir::new();
        fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        let ctx = ToolContext::new().with_cwd(dir.path().to_string_lossy().to_string());
        let input = json!({ "file_path": "lib.rs", "old_string": "a()", "new_string": "b()" });
        assert_eq!(
            tool().normalize_input(input.clone(), &ctx)["file_path"],
            json!(dir.path().join("lib.rs").display().to_string())
        );
        tool().call(input, &ctx).await.unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("lib.rs")).unwrap(),
            "fn b() {}\n"
        );
    }

    /// An edit of a file the session has seen keeps it seen, with the new
    /// content; one that creates a file makes it seen.
    #[tokio::test]
    async fn edits_keep_a_seen_file_seen_and_a_created_file_is_seen() {
        let dir = TempDir::new();
        let seen = dir.path().join("seen.txt");
        fs::write(&seen, "alpha beta").unwrap();
        let cache = FileStateCache::new();
        register_read(&cache, &seen, "alpha beta");
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(edit_input(&seen, "alpha", "ALPHA"), &ctx)
            .await
            .unwrap();
        let state = cache.get(&seen).unwrap();
        assert_eq!(state.content, "ALPHA beta");
        assert!(!state.is_partial_view);

        let created = dir.path().join("created.txt");
        tool()
            .call(edit_input(&created, "", "fresh"), &ctx)
            .await
            .unwrap();
        assert_eq!(cache.get(&created).unwrap().content, "fresh");
    }

    #[tokio::test]
    async fn edit_rejects_paths_outside_subagent_scope() {
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
            .with_path_scope_roots([inside_root.clone()]);

        let err = tool()
            .call(
                json!({
                    "file_path": target.to_string_lossy(),
                    "old_string": "hello",
                    "new_string": "bye"
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

    /// An auto-injected partial view (a truncated MEMORY.md) used to force a
    /// Read first. Edit matches the bytes on disk, so it no longer matters.
    #[tokio::test]
    async fn edit_applies_over_an_auto_injected_partial_view() {
        let dir = TempDir::new();
        let file = dir.path().join("partial.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();
        let cache = FileStateCache::new();
        cache.set(
            &file,
            FileState {
                content: "line1\nline2\nline3\n".to_string(),
                offset: None,
                limit: None,
                is_partial_view: true,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache);

        tool()
            .call(edit_input(&file, "line1", "LINE1"), &ctx)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "LINE1\nline2\nline3\n");
    }

    /// Two Edits of one file, back to back (what a response's same-file
    /// edits become once they run in order): both apply, the second
    /// against the first one's result.
    #[tokio::test]
    async fn two_edits_on_one_file_apply_back_to_back() {
        let dir = TempDir::new();
        let file = dir.path().join("chain.txt");
        fs::write(&file, "alpha beta gamma").unwrap();
        let cache = FileStateCache::new();
        register_read(&cache, &file, "alpha beta gamma");
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(edit_input(&file, "alpha", "ALPHA"), &ctx)
            .await
            .expect("first edit");
        tool()
            .call(edit_input(&file, "ALPHA beta", "ALPHA BETA"), &ctx)
            .await
            .expect("second edit matches the first edit's result");

        assert_eq!(fs::read_to_string(&file).unwrap(), "ALPHA BETA gamma");
        let state = cache.get(&file).unwrap();
        assert_eq!(state.content, "ALPHA BETA gamma");
        assert!(!state.is_partial_view);
    }

    /// A formatter, hook, or another agent changed the file after this
    /// session read it. The cached snapshot is stale, but the anchor is still
    /// unique in the current content, so the Edit applies to that content.
    #[tokio::test]
    async fn edit_applies_after_an_outside_change_when_the_anchor_is_still_unique() {
        let dir = TempDir::new();
        let file = dir.path().join("drift.txt");
        fs::write(&file, "fn main() {\n    run();\n}\n").unwrap();
        let cache = FileStateCache::new();
        register_read(&cache, &file, "fn main() {\n    run();\n}\n");
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        fs::write(&file, "// formatted\nfn main() {\n    run();\n}\n").unwrap();

        tool()
            .call(edit_input(&file, "    run();", "    run_fast();"), &ctx)
            .await
            .expect("a stale snapshot does not block an edit whose anchor still matches");

        let expected = "// formatted\nfn main() {\n    run_fast();\n}\n";
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
        assert_eq!(cache.get(&file).unwrap().content, expected);
    }

    #[tokio::test]
    async fn not_found_shows_the_closest_current_region_with_line_numbers() {
        let dir = TempDir::new();
        let file = dir.path().join("indent.rs");
        let content = "use std::fmt;\n\nfn render() {\n    let width = 80;\n    \
                       let height = 24;\n    draw(width, height);\n}\n\nfn tail() {}\n";
        fs::write(&file, content).unwrap();

        // Indented with two spaces where the file has four.
        let err = tool()
            .call(
                edit_input(
                    &file,
                    "  let width = 80;\n  let height = 24;",
                    "  let width = 120;\n  let height = 40;",
                ),
                &ToolContext::new().with_file_state_cache(FileStateCache::new()),
            )
            .await
            .unwrap_err();
        let reason = invalid_reason(err, OLD_STRING_NOT_FOUND_CODE);

        assert!(reason.contains("`old_string` was not found in"), "{reason}");
        assert!(reason.contains("lines 4-5"), "{reason}");
        assert!(reason.contains("whitespace"), "{reason}");
        for expected in [
            "     1\tuse std::fmt;",
            "     3\tfn render() {",
            "     4\t    let width = 80;",
            "     5\t    let height = 24;",
            "     8\t",
        ] {
            assert!(reason.contains(expected), "missing {expected:?}: {reason}");
        }
        assert!(
            !reason.contains("fn tail"),
            "context stays near the region: {reason}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), content);
    }

    #[tokio::test]
    async fn not_found_in_an_empty_file_says_so() {
        let dir = TempDir::new();
        let file = dir.path().join("empty.txt");
        fs::write(&file, "").unwrap();

        let err = tool()
            .call(edit_input(&file, "anything", "else"), &ToolContext::new())
            .await
            .unwrap_err();
        let reason = invalid_reason(err, OLD_STRING_NOT_FOUND_CODE);
        assert!(reason.contains("The file is empty."), "{reason}");
    }

    #[tokio::test]
    async fn ambiguous_match_lists_each_line_number() {
        let dir = TempDir::new();
        let file = dir.path().join("dupes.txt");
        fs::write(&file, "start\nretry();\nmiddle\nretry();\nend\n").unwrap();

        let err = tool()
            .call(
                edit_input(&file, "retry();", "retry(3);"),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();
        let reason = invalid_reason(err, AMBIGUOUS_MATCH_CODE);

        assert!(reason.contains("matched 2 times"), "{reason}");
        assert!(reason.contains("line 2:"), "{reason}");
        assert!(reason.contains("line 4:"), "{reason}");
        assert!(reason.contains("     1\tstart"), "{reason}");
        assert!(reason.contains("     5\tend"), "{reason}");
        assert!(reason.contains("replace_all"), "{reason}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "start\nretry();\nmiddle\nretry();\nend\n"
        );
    }

    #[tokio::test]
    async fn empty_old_string_does_not_rewrite_an_existing_file() {
        let dir = TempDir::new();
        let file = dir.path().join("existing.txt");
        fs::write(&file, "keep me").unwrap();

        let err = tool()
            .call(edit_input(&file, "", "clobber"), &ToolContext::new())
            .await
            .unwrap_err();
        invalid_reason(err, OLD_STRING_NOT_FOUND_CODE);
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep me");
    }

    #[tokio::test]
    async fn edit_fails_on_nonabsolute_path_includes_hint() {
        let err = tool()
            .call(
                json!({
                    "file_path": "relative/path.rs",
                    "old_string": "x",
                    "new_string": "y"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(INVALID_INPUT_CODE));
                assert!(
                    reason.contains("Examples"),
                    "reason should include absolute-path examples: {reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn edit_key_is_case_insensitive_on_windows() {
        // Create the file, then register it in the cache under an
        // upper-cased path and retrieve / Edit via a lower-cased one.
        let dir = TempDir::new();
        let file = dir.path().join("MixedCase.txt");
        fs::write(&file, "payload").unwrap();
        let cache = FileStateCache::new();
        register_read(&cache, &file, "payload");
        let ctx = ToolContext::new().with_file_state_cache(cache);

        // Lower-case the whole path we feed to Edit.
        let lowered: PathBuf = PathBuf::from(file.to_string_lossy().to_lowercase());
        let out = tool()
            .call(
                json!({
                    "file_path": lowered.to_string_lossy(),
                    "old_string": "payload",
                    "new_string": "PAYLOAD"
                }),
                &ctx,
            )
            .await
            .expect("case-variant path should hit same cache entry");
        assert_eq!(out["type"], json!("update"));
    }

    #[tokio::test]
    async fn call_lf_file_with_lf_strings_unchanged() {
        let dir = TempDir::new();
        let file = dir.path().join("lf.txt");
        fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let out = tool()
            .call(
                json!({
                    "file_path": file.to_string_lossy(),
                    "old_string": "alpha\nbeta",
                    "new_string": "ALPHA\nBETA"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("update"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "ALPHA\nBETA\ngamma\n");
    }
}
