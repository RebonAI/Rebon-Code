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

pub const FILE_WRITE_TOOL_NAME: &str = "Write";
const INVALID_INPUT_CODE: i64 = 400;
const NOT_FILE_CODE: i64 = 2;
const UNSUPPORTED_MEDIA_CODE: i64 = 3;
/// Writing over an existing file without having observed it via Read,
/// or when the only cached entry is an auto-injected partial view
/// (REBON.md / MEMORY.md whose processed content differs from disk).
/// Shares error semantics with Edit so the model can fix both with
/// the same action: Read, then retry.
const MUST_READ_BEFORE_WRITE_CODE: i64 = 6;
/// On-disk content diverged from the cached Read snapshot between the
/// last Read and this Write. Same semantics as Edit's errorCode 7.
const FILE_MODIFIED_SINCE_READ_CODE: i64 = 7;

#[derive(Debug, Clone, Default)]
pub struct WriteTool;

#[derive(Debug, Clone)]
struct WriteInput {
    file_path: PathBuf,
    content: String,
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
         - If this is an existing file, you MUST use the Read tool first to read the file's contents. \
         This tool will fail if you did not read the file first.\n\
         - A same-file Read and Write, or multiple mutations of the same file, are dependent \
         operations and must not be sent in one parallel tool batch. If this tool reports \
         modified-since-read, Read the file again and preserve the latest contents before retrying.\n\
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
            Ok(parsed) => check_read_before_write(&parsed.file_path, context),
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

        let state_check =
            check_read_before_write_with_state(&parsed.file_path, context, observed_state)?;
        if !state_check.is_valid() {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: state_check
                    .message
                    .unwrap_or_else(|| "Write precondition failed".into()),
                error_code: state_check.error_code,
            });
        }

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
        crate::record_file_mutation_in_current_batch(&parsed.file_path, context);

        // Refresh the cache with the just-written content so a
        // follow-up Edit on the same file doesn't trip errorCode 7.
        if let Some(cache) = context.file_state_cache() {
            let timestamp_ms = file_mtime_ms(&parsed.file_path).unwrap_or(0);
            cache.set(
                &parsed.file_path,
                FileState {
                    content: parsed.content.clone(),
                    timestamp_ms,
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
/// request well-formed. What follows differs: `validate_input` goes on to the
/// read-before-write verdict, `call` to the state-aware guard that can only
/// run where the write itself does.
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

    Ok(WriteInput {
        file_path: PathBuf::from(file_path),
        content,
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

/// Parallel to `edit::check_read_before_edit`, but tuned for Write:
///
/// * Fresh-create (file doesn't exist) → no-op. Write is the one tool
///   that's allowed to create a file without a prior Read.
/// * Updating an existing file requires that the model has observed
///   it (cache entry, full view) and that nothing has changed since.
fn check_read_before_write(path: &Path, context: &ToolContext) -> ToolResult<ValidationOutcome> {
    let observed_state = context.file_state_cache().and_then(|cache| cache.get(path));
    check_read_before_write_with_state(path, context, observed_state)
}

fn check_read_before_write_with_state(
    path: &Path,
    context: &ToolContext,
    observed_state: Option<FileState>,
) -> ToolResult<ValidationOutcome> {
    if context.file_state_cache().is_none() {
        return Ok(ValidationOutcome::valid());
    }
    if !path.exists() {
        return Ok(ValidationOutcome::valid());
    }
    match observed_state {
        None => Ok(ValidationOutcome::invalid(
            format!(
                "File has not been read yet. Use the Read tool with file_path={} first \
                 before overwriting it. Write is destructive and requires the model to \
                 have seen the current contents.",
                path.display()
            ),
            MUST_READ_BEFORE_WRITE_CODE,
        )),
        Some(state) if state.is_partial_view => Ok(ValidationOutcome::invalid(
            format!(
                "{} is only visible through an auto-injected view (REBON.md / MEMORY.md), \
                 and the injected content differs from what's on disk. Run Read on this \
                 file first to observe the real contents, then retry the Write.",
                path.display()
            ),
            MUST_READ_BEFORE_WRITE_CODE,
        )),
        Some(state) => {
            let disk = fs::read_to_string(path).ok();
            if disk.as_deref() != Some(state.content.as_str()) {
                return Ok(ValidationOutcome::invalid(
                    format!(
                        "File {} was modified since your last Read. Re-run Read \
                         before overwriting so you can merge any external changes.",
                        path.display()
                    ),
                    FILE_MODIFIED_SINCE_READ_CODE,
                ));
            }
            Ok(ValidationOutcome::valid())
        }
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

    #[tokio::test]
    async fn write_fails_when_updating_unread_file() {
        let dir = TempDir::new();
        let file = dir.path().join("existing.txt");
        fs::write(&file, "old").unwrap();
        let cache = FileStateCache::new();
        let ctx = ToolContext::new().with_file_state_cache(cache);

        let err = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "new" }),
                &ctx,
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput {
                error_code, reason, ..
            } => {
                assert_eq!(error_code, Some(MUST_READ_BEFORE_WRITE_CODE));
                assert!(
                    reason.contains("Read tool"),
                    "reason should tell the model to Read: {reason}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
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
    async fn write_create_skips_must_read_check() {
        // Fresh file that doesn't exist: Write should still succeed
        // even though there's no Read history — there's nothing to
        // observe beforehand.
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
        // Cache should have been populated as a side-effect so the next
        // Edit can proceed.
        let state = cache.get(&file).unwrap();
        assert_eq!(state.content, "fresh");
        assert!(!state.is_partial_view);
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

    #[tokio::test]
    async fn write_detects_changed_content_without_newer_mtime() {
        let dir = TempDir::new();
        let file = dir.path().join("stale.txt");
        fs::write(&file, "original").unwrap();
        let cache = FileStateCache::new();
        cache.set(
            &file,
            FileState {
                content: "original".to_string(),
                timestamp_ms: u64::MAX,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache);
        fs::write(&file, "external change").unwrap();

        let error = tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "replacement" }),
                &ctx,
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
        assert_eq!(fs::read_to_string(file).unwrap(), "external change");
    }

    #[tokio::test]
    async fn write_refreshes_cache_after_update() {
        let dir = TempDir::new();
        let file = dir.path().join("update.txt");
        fs::write(&file, "one").unwrap();
        let cache = FileStateCache::new();
        let ts = file_mtime_ms(&file).unwrap_or(0);
        cache.set(
            &file,
            FileState {
                content: "one".to_string(),
                timestamp_ms: ts,
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        let ctx = ToolContext::new().with_file_state_cache(cache.clone());

        tool()
            .call(
                json!({ "file_path": file.to_string_lossy(), "content": "two" }),
                &ctx,
            )
            .await
            .unwrap();
        let state = cache.get(&file).unwrap();
        assert_eq!(state.content, "two");
        assert_eq!(state.timestamp_ms, file_mtime_ms(&file).unwrap_or(0));
    }
}
