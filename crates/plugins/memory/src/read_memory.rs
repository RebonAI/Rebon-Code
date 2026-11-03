//! `ReadMemory`: read durable memory without file tools.
//!
//! The router has no `Read`: everything that touches a project is a
//! worker's. It still writes project memory (`SaveMemory` with `project`),
//! and to update a memory instead of adding a near-duplicate beside it, or
//! to brief a worker with what a project already knows, it has to see what
//! is there. This is that window, and nothing more: the `MEMORY.md` index,
//! or one memory it lists, of user memory or of one project's repo memory.

use crate::memory::save::{parse_scope, validate_target_file};
use crate::save_memory::{memory_cwd, project_problem};
use async_trait::async_trait;
use rebon_session::memory_paths::{memory_dir_for_scope, ENTRYPOINT_NAME};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    validation_outcome_from, PermissionDecision, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use serde_json::{json, Value};

pub const READ_MEMORY_TOOL_NAME: &str = "ReadMemory";
const INVALID_INPUT_CODE: i64 = 400;
/// Memories are short notes and an index of one-line hooks; past this much
/// text something is wrong, and the rest would only crowd the router's turn.
const MAX_CHARS: usize = 40_000;

#[derive(Debug, Clone, Default)]
pub struct ReadMemoryTool;

/// What to read: whose memory, and the index or one memory in it.
struct ReadRequest {
    scope: rebon_session::memory_paths::MemoryScope,
    project: Option<String>,
    file: Option<String>,
}

#[async_trait]
impl Tool for ReadMemoryTool {
    fn id(&self) -> ToolId {
        ToolId::new(READ_MEMORY_TOOL_NAME)
    }

    fn description(&self) -> &str {
        "Read durable memory. With no `file`, returns the MEMORY.md index of user memory \
         (scope `user`) or of a project's repo memory (scope `repo` with `project`, the \
         project's absolute path); with `file`, one memory the index lists. Read a project's \
         memory before saving to it, so you update a memory instead of adding one beside it, \
         and before briefing a worker there."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["user", "repo"],
                    "description": "Whose memory: the person's (`user`) or a project's (`repo`)."
                },
                "project": {
                    "type": "string",
                    "description": "Only with scope `repo`: the absolute path of the project whose memory to read. Omit for the project this session runs in."
                },
                "file": {
                    "type": "string",
                    "description": "One memory the index lists, by its .md file name. Omit for the index."
                }
            },
            "required": ["scope"],
            "additionalProperties": false
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(parse_input(input))
    }

    async fn check_permissions(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        Ok(PermissionDecision::allow(input.clone()))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let request = parse_input(&input)?;
        let session_cwd = context.cwd().ok_or_else(|| {
            invalid("cwd_missing: ReadMemory resolves repo memory from the session's cwd".into())
        })?;
        let cwd = memory_cwd(request.project.as_deref(), session_cwd).map_err(invalid)?;
        let dir = memory_dir_for_scope(request.scope, Some(cwd))
            .ok_or_else(|| invalid("no config home to find memory in".into()))?;
        let path = dir.join(request.file.as_deref().unwrap_or(ENTRYPOINT_NAME));
        let content = match std::fs::read_to_string(&path) {
            Ok(text) => clip(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if request.file.is_some() {
                    return Err(invalid(format!(
                        "no memory `{}` here; read the index for what there is",
                        path.display()
                    )));
                }
                "No memory is saved here yet.".to_string()
            }
            Err(error) => {
                return Err(ToolError::Execution {
                    tool: ToolId::new(READ_MEMORY_TOOL_NAME),
                    source: std::io::Error::new(
                        error.kind(),
                        format!("could not read {}: {error}", path.display()),
                    )
                    .into(),
                })
            }
        };
        Ok(json!({
            "path": path.to_string_lossy(),
            "content": content,
        }))
    }
}

fn clip(text: String) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut clipped: String = text.chars().take(MAX_CHARS).collect();
    clipped.push_str("\n\n[… cut off: this memory is longer than ReadMemory returns]");
    clipped
}

fn parse_input(input: &Value) -> ToolResult<ReadRequest> {
    let object = input
        .as_object()
        .ok_or_else(|| invalid("ReadMemory input must be an object".into()))?;
    let string = |field: &str| -> ToolResult<Option<String>> {
        match object.get(field) {
            Some(Value::String(raw)) => Ok(Some(raw.clone())),
            Some(Value::Null) | None => Ok(None),
            Some(_) => Err(invalid(format!("`{field}` must be a string"))),
        }
    };
    let scope = string("scope")?.ok_or_else(|| invalid("ReadMemory requires `scope`".into()))?;
    let scope = parse_scope(&scope).map_err(|error| invalid(error.to_string()))?;
    let project = string("project")?;
    if let Some(why) = project
        .as_deref()
        .and_then(|project| project_problem(scope, project))
    {
        return Err(invalid(why));
    }
    let file = string("file")?
        .map(|file| validate_target_file(&file))
        .transpose()
        .map_err(|error| invalid(error.to_string().replace("target_file", "`file`")))?;
    Ok(ReadRequest {
        scope,
        project,
        file,
    })
}

fn invalid(reason: String) -> ToolError {
    ToolError::InvalidInput {
        tool: ToolId::new(READ_MEMORY_TOOL_NAME),
        reason,
        error_code: Some(INVALID_INPUT_CODE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::test_env::env_test_lock;
    use crate::SaveMemoryTool;

    /// The config home (and so every memory directory) points into a temp
    /// directory for as long as this lives.
    struct Home {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl Home {
        fn new() -> Self {
            let lock = env_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let temp = tempfile::tempdir().expect("temp home");
            let previous = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("REBON_CONFIG_DIR", temp.path().join(".rebon-test"));
            Self {
                _lock: lock,
                _temp: temp,
                previous,
            }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("REBON_CONFIG_DIR", value),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
        }
    }

    fn router(workspace: &tempfile::TempDir) -> ToolContext {
        ToolContext::new().with_cwd(workspace.path().to_string_lossy().into_owned())
    }

    async fn save_lesson(project: &str, context: &ToolContext) -> String {
        let saved = SaveMemoryTool
            .call(
                json!({
                    "action": "upsert",
                    "scope": "repo",
                    "type": "project",
                    "title": "Verify with cargo test",
                    "description": "Checks run cargo test -p calc",
                    "content": "Run `cargo test -p calc` before calling a change done.",
                    "project": project,
                }),
                context,
            )
            .await
            .unwrap();
        std::path::Path::new(saved["memory_path"].as_str().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[tokio::test]
    async fn the_router_reads_a_projects_index_and_one_memory_it_lists() {
        let _home = Home::new();
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().into_owned();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);
        let file = save_lesson(&project_path, &context).await;

        let index = ReadMemoryTool
            .call(json!({"scope": "repo", "project": project_path}), &context)
            .await
            .unwrap();
        let index = index["content"].as_str().unwrap();
        assert!(index.contains("Checks run cargo test -p calc"), "{index}");
        assert!(index.contains(&file), "{index}");

        let memory = ReadMemoryTool
            .call(
                json!({"scope": "repo", "project": project_path, "file": file}),
                &context,
            )
            .await
            .unwrap();
        assert!(memory["content"]
            .as_str()
            .unwrap()
            .contains("Run `cargo test -p calc`"));
    }

    #[tokio::test]
    async fn an_empty_memory_says_so_and_a_missing_memory_is_an_error() {
        let _home = Home::new();
        let project = tempfile::tempdir().unwrap();
        let project_path = project.path().to_string_lossy().into_owned();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);

        let index = ReadMemoryTool
            .call(json!({"scope": "repo", "project": project_path}), &context)
            .await
            .unwrap();
        assert_eq!(index["content"], "No memory is saved here yet.");

        let error = ReadMemoryTool
            .call(
                json!({"scope": "repo", "project": project_path, "file": "gone.md"}),
                &context,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read the index"), "{error}");
    }

    #[tokio::test]
    async fn it_reads_only_memory_files_and_takes_project_on_save_memorys_terms() {
        let _home = Home::new();
        let workspace = tempfile::tempdir().unwrap();
        let context = router(&workspace);
        for (input, expected) in [
            (
                json!({"scope": "repo", "file": "../secrets.md"}),
                "basename",
            ),
            (json!({"scope": "repo", "file": "MEMORY.md"}), "MEMORY.md"),
            (json!({"scope": "repo", "file": "notes.txt"}), ".md"),
            (
                json!({"scope": "user", "project": "/somewhere"}),
                "only with scope `repo`",
            ),
            (
                json!({"scope": "repo", "project": "relative"}),
                "absolute path",
            ),
            (json!({"scope": "task"}), "storage scope"),
            (json!({}), "requires `scope`"),
        ] {
            let outcome = ReadMemoryTool
                .validate_input(&input, &context)
                .await
                .unwrap();
            assert!(!outcome.is_valid(), "{input}");
            let message = outcome.message.unwrap();
            assert!(message.contains(expected), "{input}: {message}");
        }
        let missing = workspace.path().join("gone");
        let error = ReadMemoryTool
            .call(
                json!({"scope": "repo", "project": missing.to_string_lossy()}),
                &context,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn a_long_memory_is_cut_and_says_so() {
        let short = "a".repeat(MAX_CHARS);
        assert_eq!(clip(short.clone()), short);
        let long = clip("b".repeat(MAX_CHARS + 10));
        assert!(long.starts_with(&"b".repeat(MAX_CHARS)));
        assert!(long.ends_with("longer than ReadMemory returns]"));
    }

    #[test]
    fn it_is_read_only_and_asks_for_no_permission() {
        let input = json!({"scope": "user"});
        assert!(ReadMemoryTool.is_read_only(&input));
        assert!(!ReadMemoryTool.needs_permission(&input));
        assert_eq!(ReadMemoryTool.input_schema()["required"], json!(["scope"]));
    }
}
