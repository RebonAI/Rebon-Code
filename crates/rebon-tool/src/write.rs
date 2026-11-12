use crate::{Tool, ToolContext};
use async_trait::async_trait;
use rebon_tools_core::{
    file_state::FileState, require_valid_input, validation_outcome_from, PermissionDecision,
    PermissionRequest, ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE_WRITE_TOOL_NAME: &str = "Write";
const INVALID_INPUT_CODE: i64 = 400;
const NOT_FILE_CODE: i64 = 2;
const UNSUPPORTED_MEDIA_CODE: i64 = 3;
/// Overwriting an existing file this session has not seen, without
/// `overwrite: true`.
pub(crate) const UNSEEN_OVERWRITE_CODE: i64 = 6;

#[derive(Debug, Clone, Default)]
pub struct WriteTool;

#[derive(Debug, Clone)]
struct WriteInput {
    file_path: PathBuf,
    content: String,
    /// Replace an existing file even though this session has not seen it.
    overwrite: bool,
}

#[async_trait]
impl Tool for WriteTool {
    fn id(&self) -> ToolId {
        ToolId::new(FILE_WRITE_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["FileWriteTool"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::FileEdit
    }

    fn file_target_field(&self) -> Option<&'static str> {
        Some("file_path")
    }

    fn description(&self) -> &str {
        "Writes a file to the local filesystem.\n\
         \n\
         Usage:\n\
         - This tool will overwrite the existing file if there is one at the provided path.\n\
         - If this is an existing file, Read it first or pass `overwrite: true`; otherwise this tool fails.\n\
         - Prefer the Edit tool for modifying existing files \u{2014} it only sends the diff. \
         Only use this tool to create new files or for complete rewrites.\n\
         - NEVER create documentation files (*.md) or README files unless explicitly requested by the User.\n\
         - Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace an existing file you have not read in this session"
                }
            },
            "required": ["file_path", "content"],
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
            Ok(parsed) => Ok(check_overwrite(&parsed, context)),
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

        if crate::edit::explicitly_authorized_write_path(Path::new(path), context) {
            return Ok(PermissionDecision::allow(input.clone()));
        }

        // Auto-allow writes to the auto-memory directory unless the
        // context carries an explicit narrower write scope.
        if context.write_scope_roots().is_none() {
            if let Some(cwd) = context.cwd() {
                if rebon_session::memory_paths::is_memory_path_for_any_scope(path, cwd) {
                    return Ok(PermissionDecision::allow(input.clone()));
                }
            }
        }

        Ok(PermissionDecision::ask(
            PermissionRequest::new(
                "Write file",
                format!("Write wants to create or overwrite: {path}"),
            )
            .with_options(["allow_once", "allow_always", "reject_once"]),
            Some(input.clone()),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = prepare_input(self.id(), &input, context)?;

        // Decided under the path lock, so "does the file exist yet" and the
        // write that follows cannot be split by another writer.
        let _file_guard = crate::lock_file_for_write(&parsed.file_path).await;
        require_valid_input(
            self.id(),
            check_overwrite(&parsed, context),
            "Write precondition failed",
        )?;

        let old_content = match fs::read(&parsed.file_path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(ToolError::Execution {
                    tool: self.id(),
                    source: err.into(),
                })
            }
        };

        let original_text = match old_content {
            Some(bytes) => Some(
                String::from_utf8(bytes).map_err(|_| ToolError::InvalidInput {
                    tool: self.id(),
                    reason: format!(
                        "Unsupported non-text file in current Rust Write slice: {}",
                        parsed.file_path.display()
                    ),
                    error_code: Some(UNSUPPORTED_MEDIA_CODE),
                })?,
            ),
            None => None,
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

        fs::write(&parsed.file_path, parsed.content.as_bytes()).map_err(|err| {
            ToolError::Execution {
                tool: self.id(),
                source: err.into(),
            }
        })?;

        // Record what this session just wrote: a later Write of the same
        // file needs no `overwrite`, and Read's range merge sees current
        // content.
        if let Some(cache) = context.file_state_cache() {
            cache.set(
                &parsed.file_path,
                FileState {
                    content: parsed.content.clone(),
                    offset: None,
                    limit: None,
                    is_partial_view: false,
                },
            );
        }

        let write_type = if original_text.is_some() {
            "update"
        } else {
            "create"
        };

        let result = json!({
            "type": write_type,
            "filePath": normalize_path(&parsed.file_path),
            "content": parsed.content,
            // Match Edit tool's result shape so the TUI renders a diff view.
            "oldString": original_text.as_deref().unwrap_or(""),
            "newString": parsed.content,
            "originalFile": original_text,
            "numLines": count_lines(input_string(&input, "content").unwrap_or_default()),
        });

        Ok(result)
    }
}

/// Parse and vet one `Write` request, once.
///
/// Both entry points ask the same three questions in the same order — does
/// the input parse, does the path clear the write scope, is the parsed
/// request well-formed. Both then ask [`check_overwrite`]; only `call`'s
/// answer, given under the path lock, is decisive.
fn prepare_input(tool: ToolId, input: &Value, context: &ToolContext) -> ToolResult<WriteInput> {
    let parsed = parse_input(input)?;
    enforce_write_path_scope(tool.clone(), &parsed.file_path, context)?;
    require_valid_input(
        tool,
        validate_parsed_input(&parsed)?,
        "Write input is invalid",
    )?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<WriteInput> {
    let tool = ToolId::new(FILE_WRITE_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Write input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let file_path = required_string(object.get("file_path"), "file_path", &tool)?;
    let content = required_string(object.get("content"), "content", &tool)?;
    let overwrite =
        crate::edit::optional_bool(object.get("overwrite"), "overwrite", &tool)?.unwrap_or(false);

    Ok(WriteInput {
        file_path: PathBuf::from(file_path),
        content,
        overwrite,
    })
}

fn validate_parsed_input(input: &WriteInput) -> ToolResult<ValidationOutcome> {
    if !input.file_path.is_absolute() {
        return Ok(ValidationOutcome::invalid(
            format!(
                "Write requires an absolute `file_path`, got: {}. \
                 Examples: C:\\Users\\name\\file.rs or D:/project/file.rs \
                 on Windows; /home/name/file.rs on Linux/macOS.",
                input.file_path.display()
            ),
            INVALID_INPUT_CODE,
        ));
    }

    match fs::metadata(&input.file_path) {
        Ok(metadata) if !metadata.is_file() => Ok(ValidationOutcome::invalid(
            format!("Path is not a file: {}", input.file_path.display()),
            NOT_FILE_CODE,
        )),
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ValidationOutcome::valid()),
        Err(err) => Err(ToolError::Execution {
            tool: ToolId::new(FILE_WRITE_TOOL_NAME),
            source: err.into(),
        }),
    }
}

fn required_string(value: Option<&Value>, field: &str, tool: &ToolId) -> ToolResult<String> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: format!("Write input requires a string `{field}`"),
            error_code: Some(INVALID_INPUT_CODE),
        })
        .map(ToOwned::to_owned)
}

fn input_string(input: &Value, field: &str) -> Option<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_auto_memory_path(path: &Path, context: &ToolContext) -> bool {
    context.cwd().is_some_and(|cwd| {
        rebon_session::memory_paths::is_memory_path_for_any_scope(&path.to_string_lossy(), cwd)
    })
}

/// Sub-agent mutation-scope gate for writes. An explicit write scope
/// is authoritative, including over the ordinary auto-memory carve-out.
fn enforce_write_path_scope(
    tool: rebon_tools_core::ToolId,
    path: &Path,
    context: &ToolContext,
) -> ToolResult<()> {
    if context.write_scope_roots().is_none() && is_auto_memory_path(path, context) {
        return Ok(());
    }
    crate::path_scope::enforce_write_path_policy(tool, context, path, "file_path")
}

/// Whether this Write may replace what is at `input.file_path`.
///
/// * No file-state cache on the context → allowed (contexts outside a
///   session, which have nothing to consult).
/// * The file does not exist → allowed: creating needs nothing.
/// * `overwrite: true` → allowed.
/// * This session has seen the file — a Read, its own Edit/Write, or a
///   verbatim prompt injection — → allowed. Seen is a cache entry that is
///   not an auto-injected partial view; the content is never compared, so a
///   formatter or another writer changing the file since is not a refusal.
/// * Otherwise refused: replacing a file the model never looked at is the
///   one blind overwrite this guards against.
fn check_overwrite(input: &WriteInput, context: &ToolContext) -> ValidationOutcome {
    let Some(cache) = context.file_state_cache() else {
        return ValidationOutcome::valid();
    };
    let path = &input.file_path;
    if input.overwrite || !path.exists() {
        return ValidationOutcome::valid();
    }
    match cache.get(path) {
        Some(state) if !state.is_partial_view => ValidationOutcome::valid(),
        Some(_) => ValidationOutcome::invalid(
            format!(
                "{} already exists and was only shown in a shortened auto-injected view \
                 (REBON.md / MEMORY.md). Read it first, or pass `overwrite: true` to replace it.",
                path.display()
            ),
            UNSEEN_OVERWRITE_CODE,
        ),
        None => ValidationOutcome::invalid(
            format!(
                "{} already exists and has not been read in this session. Read it first, or \
                 pass `overwrite: true` to replace it.",
                path.display()
            ),
            UNSEEN_OVERWRITE_CODE,
        ),
    }
}

fn count_lines(content: String) -> usize {
    if content.is_empty() {
        0
    } else {
        content.lines().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::test_support::TestConfigHome;
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
                    .prefix("rebon-write-tool-test-")
                    .tempdir()
                    .unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }
    }

    fn tool() -> WriteTool {
        WriteTool
    }

    #[tokio::test]
    async fn validate_input_rejects_relative_path() {
        let input = json!({ "file_path": "notes.txt", "content": "hello" });
        let result = tool()
            .validate_input(&input, &ToolContext::new())
            .await
            .unwrap();
        assert!(!result.is_valid());
        assert_eq!(result.error_code, Some(INVALID_INPUT_CODE));
    }

    #[tokio::test]
    async fn call_creates_new_file() {
        let dir = TempDir::new();
        let file = dir.path().join("new.txt");
        let out = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "alpha\nbeta" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("create"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "alpha\nbeta");
        assert_eq!(out["originalFile"], Value::Null);
    }

    #[tokio::test]
    async fn call_updates_existing_file() {
        let dir = TempDir::new();
        let file = dir.path().join("existing.txt");
        fs::write(&file, "old").unwrap();

        let out = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "new" }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["type"], json!("update"));
        assert_eq!(out["originalFile"], json!("old"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
    }

    fn assert_unseen_overwrite_refused(err: ToolError) -> String {
        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(UNSEEN_OVERWRITE_CODE), "{reason}");
                assert!(reason.contains("Read it first"), "{reason}");
                assert!(reason.contains("`overwrite: true`"), "{reason}");
                reason
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_over_an_unseen_existing_file_is_refused() {
        let dir = TempDir::new();
        let file = dir.path().join("existing.txt");
        fs::write(&file, "old").unwrap();
        let ctx = ToolContext::new().with_file_state_cache(FileStateCache::new());
        let input = json!({ "file_path": file.to_string_lossy(), "content": "new" });

        let verdict = tool().validate_input(&input, &ctx).await.unwrap();
        assert_eq!(verdict.error_code, Some(UNSEEN_OVERWRITE_CODE));
        let err = tool().call(input, &ctx).await.unwrap_err();
        let reason = assert_unseen_overwrite_refused(err);
        assert!(reason.contains("has not been read"), "{reason}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "old");
    }

    #[tokio::test]
    async fn write_over_an_existing_file_succeeds_after_a_read() {
        let dir = TempDir::new();
        let file = dir.path().join("existing.txt");
        fs::write(&file, "old").unwrap();
        let ctx = ToolContext::new().with_file_state_cache(FileStateCache::new());

        crate::ReadTool
            .call(json!({ "targets": [{ "path": file }] }), &ctx)
            .await
            .unwrap();
        let out = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "new" }),
                &ctx,
            )
            .await
            .expect("a Read marks the file as seen");
        assert_eq!(out["type"], json!("update"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
    }

    #[tokio::test]
    async fn write_over_an_unseen_existing_file_succeeds_with_overwrite() {
        let dir = TempDir::new();
        let file = dir.path().join("existing.txt");
        fs::write(&file, "old").unwrap();
        let ctx = ToolContext::new().with_file_state_cache(FileStateCache::new());
        let input = json!({
            "file_path": file.to_string_lossy(),
            "content": "new",
            "overwrite": true
        });

        assert!(tool()
            .validate_input(&input, &ctx)
            .await
            .unwrap()
            .is_valid());
        tool().call(input, &ctx).await.unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
    }

    #[tokio::test]
    async fn overwrite_must_be_a_boolean() {
        let dir = TempDir::new();
        let file = dir.path().join("existing.txt");
        let err = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "x", "overwrite": "yes" }),
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
        assert!(!file.exists());
    }

    /// A truncated MEMORY.md shown in the prompt is not "seen".
    #[tokio::test]
    async fn an_auto_injected_partial_view_does_not_count_as_seen() {
        let dir = TempDir::new();
        let file = dir.path().join("MEMORY.md");
        fs::write(&file, "long index").unwrap();
        let cache = FileStateCache::new();
        cache.set(
            &file,
            FileState {
                content: "long index".to_string(),
                offset: None,
                limit: None,
                is_partial_view: true,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache);

        let err = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "short" }),
                &ctx,
            )
            .await
            .unwrap_err();
        let reason = assert_unseen_overwrite_refused(err);
        assert!(reason.contains("auto-injected"), "{reason}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "long index");
    }

    #[tokio::test]
    async fn write_rejects_paths_outside_subagent_scope() {
        let dir = TempDir::new();
        let inside_root = dir.path().join("scope");
        let outside = dir.path().join("elsewhere");
        fs::create_dir_all(&inside_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("agent-scope")
            .with_cwd(inside_root.to_string_lossy().to_string())
            .with_path_scope_roots([inside_root.clone()]);

        let escape = outside.join("escape.txt");
        let err = tool()
            .call(
                json!({ "file_path": escape.to_string_lossy(), "content": "nope" }),
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
        assert!(!escape.exists());

        let out = tool()
            .call(
                json!({
                    "file_path": inside_root.join("ok.txt").to_string_lossy(),
                    "content": "fine"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["type"], json!("create"));
    }

    #[tokio::test]
    async fn scratchpad_auto_approval_does_not_auto_approve_project_writes() {
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

        let scratchpad_input =
            json!({ "file_path": scratchpad.join("result.txt"), "content": "artifact" });
        let project_input = json!({ "file_path": project.join("src.txt"), "content": "project" });

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
    async fn scratchpad_auto_approval_rejects_hard_link_alias() {
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        let outside = dir.path().join("outside.txt");
        let alias = scratchpad.join("alias.txt");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        fs::write(&outside, "outside").unwrap();
        fs::hard_link(&outside, &alias).unwrap();
        let ctx = ToolContext::new()
            .with_agent_id("general-purpose")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_auto_approved_write_roots([scratchpad]);
        let input = json!({ "file_path": alias, "content": "escaped" });

        assert_eq!(
            tool()
                .check_permissions(&input, &ctx)
                .await
                .unwrap()
                .behavior,
            PermissionBehavior::Ask
        );
        assert!(matches!(
            tool().call(input, &ctx).await,
            Err(ToolError::InvalidInput { .. })
        ));
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
    }

    #[tokio::test]
    async fn explicit_write_scope_allows_scratchpad_without_permission_prompt() {
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let file = scratchpad.join("result.txt");
        let ctx = ToolContext::new()
            .with_agent_id("verification")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);
        let input = json!({ "file_path": file.to_string_lossy(), "content": "verified" });

        let decision = tool().check_permissions(&input, &ctx).await.unwrap();
        assert_eq!(decision.behavior, PermissionBehavior::Allow);
        tool().call(input, &ctx).await.unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "verified");
    }

    /// A root the session may write to unasked stops at git metadata: a
    /// scratchpad that became a repository has a `.git/config` the next
    /// unasked `git status` would run.
    #[tokio::test]
    async fn an_authorized_root_does_not_cover_git_metadata() {
        let dir = TempDir::new();
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(scratchpad.join(".git")).unwrap();
        for ctx in [
            ToolContext::new()
                .with_cwd(scratchpad.to_string_lossy())
                .with_auto_approved_write_roots([scratchpad.clone()]),
            ToolContext::new()
                .with_cwd(scratchpad.to_string_lossy())
                .with_write_scope_roots([scratchpad.clone()]),
        ] {
            for target in [scratchpad.join(".git/config"), scratchpad.join(".git")] {
                let input = json!({ "file_path": target.to_string_lossy(), "content": "x" });
                let decision = tool().check_permissions(&input, &ctx).await.unwrap();
                assert_eq!(decision.behavior, PermissionBehavior::Ask, "{target:?}");
            }
            let input = json!({
                "file_path": scratchpad.join("notes.txt").to_string_lossy(),
                "content": "x"
            });
            let decision = tool().check_permissions(&input, &ctx).await.unwrap();
            assert_eq!(decision.behavior, PermissionBehavior::Allow);
        }
    }

    #[tokio::test]
    async fn explicit_write_scope_rejects_project_writes() {
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let file = project.join("src.txt");
        let ctx = ToolContext::new()
            .with_agent_id("verification")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);

        let result = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "nope" }),
                &ctx,
            )
            .await;

        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn explicit_write_scope_rejects_auto_memory_carveout() {
        let _config_home = TestConfigHome::new("write-explicit-scope-memory");
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let scratchpad = dir.path().join("scratchpad");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&scratchpad).unwrap();
        let memory_file = rebon_session::memory_paths::repo_memory_dir(&project.to_string_lossy())
            .unwrap()
            .join("user.md");
        let ctx = ToolContext::new()
            .with_agent_id("verification")
            .with_cwd(project.to_string_lossy())
            .with_path_scope_roots([project, scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);

        let result = tool()
            .call(
                json!({ "file_path": memory_file.to_string_lossy(), "content": "nope" }),
                &ctx,
            )
            .await;

        assert!(matches!(result, Err(ToolError::InvalidInput { .. })));
        assert!(!memory_file.exists());
    }

    #[tokio::test]
    async fn write_creating_a_new_file_needs_nothing() {
        // Fresh file that doesn't exist: no Read history and no
        // `overwrite` — there's nothing to observe beforehand.
        let dir = TempDir::new();
        let file = dir.path().join("brand_new.txt");
        let cache = FileStateCache::new();
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        let out = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "fresh" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["type"], json!("create"));
        // The write marks the file as seen, so a second Write of it needs
        // no `overwrite`.
        let state = cache.get(&file).unwrap();
        assert_eq!(state.content, "fresh");
        assert!(!state.is_partial_view);
        tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "fresher" }),
                &ctx,
            )
            .await
            .expect("the session's own Write counts as seen");
        assert_eq!(fs::read_to_string(&file).unwrap(), "fresher");
    }

    #[tokio::test]
    async fn write_to_auto_memory_path_is_permission_allowed() {
        let config_home = TestConfigHome::new("write-memory-permission");
        let dir = TempDir::new();
        let cwd = dir.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let memory_file = rebon_session::memory_paths::repo_memory_dir(&cwd.to_string_lossy())
            .unwrap()
            .join("user.md");
        let input = json!({ "file_path": memory_file.to_string_lossy(), "content": "remember" });
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
    async fn write_to_auto_memory_path_skips_file_history_tracking() {
        let _config_home = TestConfigHome::new("write-memory-history-skip");
        let dir = TempDir::new();
        let cwd = dir.path().join("project");
        fs::create_dir_all(&cwd).unwrap();
        let memory_file = rebon_session::memory_paths::repo_memory_dir(&cwd.to_string_lossy())
            .unwrap()
            .join("user.md");
        let tracker = Arc::new(RecordingHistoryTracker::default());
        let ctx = ToolContext::new()
            .with_cwd(cwd.to_string_lossy())
            .with_file_history_tracker(tracker.clone());

        tool()
            .call(
                json!({ "file_path": memory_file.to_string_lossy(), "content": "remember" }),
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

    /// Seen is about presence, not freshness: the file changed after this
    /// session read it, and the Write still goes through.
    #[tokio::test]
    async fn write_does_not_compare_content_with_the_cached_snapshot() {
        let dir = TempDir::new();
        let file = dir.path().join("stale.txt");
        fs::write(&file, "original").unwrap();
        let cache = FileStateCache::new();
        cache.set(
            &file,
            FileState {
                content: "original".to_string(),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());
        fs::write(&file, "external change").unwrap();

        tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "replacement" }),
                &ctx,
            )
            .await
            .expect("a changed file the session has seen may be overwritten");
        assert_eq!(fs::read_to_string(&file).unwrap(), "replacement");
        let state = cache.get(&file).unwrap();
        assert_eq!(state.content, "replacement");
        assert!(state.offset.is_none() && state.limit.is_none());
    }
}
